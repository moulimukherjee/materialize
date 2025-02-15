// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Integration tests for TLS encryption and authentication.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::future::IntoFuture;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use headers::Authorization;
use hyper::client::HttpConnector;
use hyper::http::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use hyper::http::uri::Scheme;
use hyper::{body, Body, Request, Response, StatusCode, Uri};
use hyper_openssl::HttpsConnector;
use jsonwebtoken::{self, DecodingKey, EncodingKey};
use mz_environmentd::test_util::{self, make_header, make_pg_tls, Ca};
use mz_environmentd::{WebSocketAuth, WebSocketResponse};
use mz_frontegg_auth::{
    Authentication as FronteggAuthentication, AuthenticationConfig as FronteggConfig, Claims,
};
use mz_frontegg_mock::FronteggMockServer;
use mz_ore::assert_contains;
use mz_ore::metrics::MetricsRegistry;
use mz_ore::now::{NowFn, SYSTEM_TIME};
use mz_ore::retry::Retry;
use mz_sql::names::PUBLIC_ROLE_NAME;
use mz_sql::session::user::{HTTP_DEFAULT_USER, SYSTEM_USER};
use openssl::error::ErrorStack;
use openssl::ssl::{SslConnector, SslConnectorBuilder, SslMethod, SslOptions, SslVerifyMode};
use postgres::config::SslMode;
use postgres::error::SqlState;
use serde::Deserialize;
use serde_json::json;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::Message;
use uuid::Uuid;

// How long, in seconds, a claim is valid for. Increasing this value will decrease some test flakes
// without increasing test time.
const EXPIRES_IN_SECS: u64 = 20;

fn make_http_tls<F>(configure: F) -> HttpsConnector<HttpConnector>
where
    F: Fn(&mut SslConnectorBuilder) -> Result<(), ErrorStack>,
{
    let mut connector_builder = SslConnector::builder(SslMethod::tls()).unwrap();
    // See comment in `make_pg_tls` about disabling TLS v1.3.
    let options = connector_builder.options() | SslOptions::NO_TLSV1_3;
    connector_builder.set_options(options);
    configure(&mut connector_builder).unwrap();
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    HttpsConnector::with_connector(http, connector_builder).unwrap()
}

fn make_ws_tls<F>(uri: &Uri, configure: F) -> impl Read + Write
where
    F: Fn(&mut SslConnectorBuilder) -> Result<(), ErrorStack>,
{
    let mut connector_builder = SslConnector::builder(SslMethod::tls()).unwrap();
    // See comment in `make_pg_tls` about disabling TLS v1.3.
    let options = connector_builder.options() | SslOptions::NO_TLSV1_3;
    connector_builder.set_options(options);
    configure(&mut connector_builder).unwrap();
    let connector = connector_builder.build();

    let stream =
        TcpStream::connect(format!("{}:{}", uri.host().unwrap(), uri.port().unwrap())).unwrap();
    connector.connect(uri.host().unwrap(), stream).unwrap()
}

// Use two error types because some tests need to retry certain errors because
// there's a race condition for which is produced and they always need a
// postgres-style error.
enum Assert<E, D = ()> {
    Success,
    SuccessSuperuserCheck(bool),
    Err(E),
    DbErr(D),
}

enum TestCase<'a> {
    Pgwire {
        user_to_auth_as: &'a str,
        user_reported_by_system: &'a str,
        password: Option<&'a str>,
        ssl_mode: SslMode,
        configure: Box<dyn Fn(&mut SslConnectorBuilder) -> Result<(), ErrorStack> + 'a>,
        assert: Assert<
            // A non-retrying, raw error.
            Box<dyn Fn(&tokio_postgres::error::Error) + 'a>,
            // A check that retries until it gets a DbError.
            Box<dyn Fn(&tokio_postgres::error::DbError) + 'a>,
        >,
    },
    Http {
        user_to_auth_as: &'a str,
        user_reported_by_system: &'a str,
        scheme: Scheme,
        headers: &'a HeaderMap,
        configure: Box<dyn Fn(&mut SslConnectorBuilder) -> Result<(), ErrorStack> + 'a>,
        assert: Assert<Box<dyn Fn(Option<StatusCode>, String) + 'a>>,
    },
    Ws {
        auth: &'a WebSocketAuth,
        configure: Box<dyn Fn(&mut SslConnectorBuilder) -> Result<(), ErrorStack> + 'a>,
        assert: Assert<Box<dyn Fn(CloseCode, String) + 'a>>,
    },
}

fn assert_http_rejected() -> Assert<Box<dyn Fn(Option<StatusCode>, String)>> {
    Assert::Err(Box::new(|code, message| {
        const ALLOWED_MESSAGES: [&str; 2] = [
            "Connection reset by peer",
            "connection closed before message completed",
        ];
        assert_eq!(code, None);
        if !ALLOWED_MESSAGES
            .iter()
            .any(|allowed| message.contains(allowed))
        {
            panic!("TLS rejected with unexpected error message: {}", message)
        }
    }))
}

async fn run_tests<'a>(header: &str, server: &test_util::TestServer, tests: &[TestCase<'a>]) {
    println!("==> {}", header);
    for test in tests {
        match test {
            TestCase::Pgwire {
                user_to_auth_as,
                user_reported_by_system,
                password,
                ssl_mode,
                configure,
                assert,
            } => {
                println!(
                    "pgwire user={} password={:?} ssl_mode={:?}",
                    user_to_auth_as, password, ssl_mode
                );

                let tls = make_pg_tls(configure);
                let conn_config = server
                    .connect()
                    .ssl_mode(*ssl_mode)
                    .user(user_to_auth_as)
                    .password(password.unwrap_or(""));

                match assert {
                    Assert::Success => {
                        let pg_client = conn_config.with_tls(tls).await.unwrap();
                        let row = pg_client
                            .query_one("SELECT current_user", &[])
                            .await
                            .unwrap();
                        assert_eq!(row.get::<_, String>(0), *user_reported_by_system);
                    }
                    Assert::SuccessSuperuserCheck(is_superuser) => {
                        let pg_client = conn_config.with_tls(tls).await.unwrap();
                        let row = pg_client
                            .query_one("SELECT current_user", &[])
                            .await
                            .unwrap();
                        assert_eq!(row.get::<_, String>(0), *user_reported_by_system);

                        let row = pg_client.query_one("SHOW is_superuser", &[]).await.unwrap();
                        let expected = if *is_superuser { "on" } else { "off" };
                        assert_eq!(row.get::<_, String>(0), *expected);
                    }
                    Assert::DbErr(check) => {
                        // This sometimes returns a network error, so retry until we get a db error.
                        Retry::default()
                            .max_duration(Duration::from_secs(10))
                            .retry_async(|_| async {
                                let Err(err) = server
                                    .connect()
                                    .with_config(conn_config.as_pg_config().clone())
                                    .with_tls(tls.clone())
                                    .await
                                else {
                                    return Err(());
                                };
                                let Some(err) = err.as_db_error() else {
                                    return Err(());
                                };
                                check(err);
                                Ok(())
                            })
                            .await
                            .unwrap();
                    }
                    Assert::Err(check) => {
                        let pg_client = conn_config.with_tls(tls.clone()).await;
                        let err = match pg_client {
                            Ok(_) => panic!("connection unexpectedly succeeded"),
                            Err(err) => err,
                        };
                        check(&err);
                    }
                }
            }
            TestCase::Http {
                user_to_auth_as,
                user_reported_by_system,
                scheme,
                headers,
                configure,
                assert,
            } => {
                async fn query_http_api<'a>(
                    query: &str,
                    uri: &Uri,
                    headers: &'a HeaderMap,
                    configure: &Box<
                        dyn Fn(&mut SslConnectorBuilder) -> Result<(), ErrorStack> + 'a,
                    >,
                ) -> hyper::Result<Response<Body>> {
                    hyper::Client::builder()
                        .build::<_, Body>(make_http_tls(configure))
                        .request({
                            let mut req = Request::post(uri);
                            for (k, v) in headers.iter() {
                                req.headers_mut().unwrap().insert(k, v.clone());
                            }
                            req.headers_mut().unwrap().insert(
                                "Content-Type",
                                HeaderValue::from_static("application/json"),
                            );
                            req.body(Body::from(json!({ "query": query }).to_string()))
                                .unwrap()
                        })
                        .await
                }

                async fn assert_success_response(
                    res: hyper::Result<Response<Body>>,
                    expected_rows: Vec<Vec<String>>,
                ) {
                    #[derive(Deserialize)]
                    struct Result {
                        rows: Vec<Vec<String>>,
                    }
                    #[derive(Deserialize)]
                    struct Response {
                        results: Vec<Result>,
                    }
                    let body = body::to_bytes(res.unwrap().into_body()).await.unwrap();
                    let res: Response = serde_json::from_slice(&body).unwrap();
                    assert_eq!(res.results[0].rows, expected_rows)
                }

                println!("http user={} scheme={}", user_to_auth_as, scheme);

                let uri = Uri::builder()
                    .scheme(scheme.clone())
                    .authority(&*format!(
                        "{}:{}",
                        Ipv4Addr::LOCALHOST,
                        server.inner.http_local_addr().port()
                    ))
                    .path_and_query("/api/sql")
                    .build()
                    .unwrap();
                let res =
                    query_http_api("SELECT pg_catalog.current_user()", &uri, headers, configure)
                        .await;

                match assert {
                    Assert::Success => {
                        assert_success_response(
                            res,
                            vec![vec![user_reported_by_system.to_string()]],
                        )
                        .await;
                    }
                    Assert::SuccessSuperuserCheck(is_superuser) => {
                        assert_success_response(
                            res,
                            vec![vec![user_reported_by_system.to_string()]],
                        )
                        .await;
                        let res =
                            query_http_api("SHOW is_superuser", &uri, headers, configure).await;
                        let expected = if *is_superuser { "on" } else { "off" };
                        assert_success_response(res, vec![vec![expected.to_string()]]).await;
                    }
                    Assert::Err(check) => {
                        let (code, message) = match res {
                            Ok(mut res) => {
                                let body = body::to_bytes(res.body_mut()).await.unwrap();
                                let body = String::from_utf8_lossy(&body[..]).into_owned();
                                (Some(res.status()), body)
                            }
                            Err(e) => (None, e.to_string()),
                        };
                        check(code, message)
                    }
                    Assert::DbErr(_) => unreachable!(),
                }
            }
            TestCase::Ws {
                auth,
                configure,
                assert,
            } => {
                println!("ws auth={:?}", auth);

                let uri = Uri::builder()
                    .scheme("wss")
                    .authority(&*format!(
                        "{}:{}",
                        Ipv4Addr::LOCALHOST,
                        server.inner.http_local_addr().port()
                    ))
                    .path_and_query("/api/experimental/sql")
                    .build()
                    .unwrap();
                let stream = make_ws_tls(&uri, configure);
                let (mut ws, _resp) = tungstenite::client(uri, stream).unwrap();

                ws.send(Message::Text(serde_json::to_string(&auth).unwrap()))
                    .unwrap();

                ws.send(Message::Text(
                    r#"{"query": "SELECT pg_catalog.current_user()"}"#.into(),
                ))
                .unwrap();

                // Only supports reading a single row.
                fn assert_success_response(
                    ws: &mut tungstenite::WebSocket<impl Read + Write>,
                    mut expected_row_opt: Option<Vec<&str>>,
                    mut expected_tag_opt: Option<&str>,
                ) {
                    while expected_tag_opt.is_some() || expected_row_opt.is_some() {
                        let resp = ws.read().unwrap();
                        if let Message::Text(msg) = resp {
                            let msg: WebSocketResponse = serde_json::from_str(&msg).unwrap();
                            match (msg, &expected_row_opt, expected_tag_opt) {
                                (WebSocketResponse::Row(actual_row), Some(expected_row), _) => {
                                    assert_eq!(actual_row.len(), expected_row.len());
                                    for (actual_col, expected_col) in
                                        actual_row.into_iter().zip(expected_row.iter())
                                    {
                                        assert_eq!(&actual_col.to_string(), expected_col);
                                    }
                                    expected_row_opt = None;
                                }
                                (
                                    WebSocketResponse::CommandComplete(actual_tag),
                                    _,
                                    Some(expected_tag),
                                ) => {
                                    assert_eq!(actual_tag, expected_tag);
                                    expected_tag_opt = None;
                                }
                                (_, _, _) => {}
                            }
                        } else {
                            panic!("unexpected: {resp}");
                        }
                    }
                }

                match assert {
                    Assert::Success => assert_success_response(&mut ws, None, Some("SELECT 1")),
                    Assert::SuccessSuperuserCheck(is_superuser) => {
                        assert_success_response(&mut ws, None, Some("SELECT 1"));
                        ws.send(Message::Text(r#"{"query": "SHOW is_superuser"}"#.into()))
                            .unwrap();
                        let expected = if *is_superuser { "\"on\"" } else { "\"off\"" };
                        assert_success_response(&mut ws, Some(vec![expected]), Some("SELECT 1"));
                    }
                    Assert::Err(check) => {
                        let resp = ws.read().unwrap();
                        let (code, message) = match resp {
                            Message::Close(frame) => {
                                let frame = frame.unwrap();
                                (frame.code, frame.reason)
                            }
                            _ => panic!("unexpected: {resp}"),
                        };
                        check(code, message.to_string())
                    }
                    Assert::DbErr(_) => unreachable!(),
                }
            }
        }
    }
}

#[mz_ore::test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `OPENSSL_init_ssl` on OS `linux`
async fn test_auth_expiry() {
    // This function verifies that the background expiry refresh task runs. This
    // is done by starting a web server that awaits the refresh request, which the
    // test waits for.

    let ca = Ca::new_root("test ca").unwrap();
    let (server_cert, server_key) = ca
        .request_cert("server", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .unwrap();
    let metrics_registry = MetricsRegistry::new();

    let tenant_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let secret = Uuid::new_v4();
    let users = BTreeMap::from([(
        (client_id.to_string(), secret.to_string()),
        "user@_.com".to_string(),
    )]);
    let roles = BTreeMap::from([("user@_.com".to_string(), Vec::new())]);
    let encoding_key =
        EncodingKey::from_rsa_pem(&ca.pkey.private_key_to_pem_pkcs8().unwrap()).unwrap();

    let frontegg_server = FronteggMockServer::start(
        None,
        encoding_key,
        tenant_id,
        users,
        roles,
        SYSTEM_TIME.clone(),
        i64::try_from(EXPIRES_IN_SECS).unwrap(),
        None,
    )
    .unwrap();

    let frontegg_auth = FronteggAuthentication::new(
        FronteggConfig {
            admin_api_token_url: frontegg_server.url.clone(),
            decoding_key: DecodingKey::from_rsa_pem(&ca.pkey.public_key_to_pem().unwrap()).unwrap(),
            tenant_id: Some(tenant_id),
            now: SYSTEM_TIME.clone(),
            admin_role: "mzadmin".to_string(),
        },
        mz_frontegg_auth::Client::default(),
        &metrics_registry,
    );
    let frontegg_user = "user@_.com";
    let frontegg_password = &format!("mzp_{client_id}{secret}");

    let server = test_util::TestHarness::default()
        .with_tls(server_cert, server_key)
        .with_frontegg(&frontegg_auth)
        .with_metrics_registry(metrics_registry)
        .start()
        .await;

    let pg_client = server
        .connect()
        .ssl_mode(SslMode::Require)
        .user(frontegg_user)
        .password(frontegg_password)
        .with_tls(make_pg_tls(Box::new(|b: &mut SslConnectorBuilder| {
            Ok(b.set_verify(SslVerifyMode::NONE))
        })))
        .await
        .unwrap();

    assert_eq!(
        pg_client
            .query_one("SELECT current_user", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        frontegg_user
    );

    // Wait for a couple refreshes to happen.
    frontegg_server.wait_for_refresh(EXPIRES_IN_SECS);
    frontegg_server.wait_for_refresh(EXPIRES_IN_SECS);
    assert_eq!(
        pg_client
            .query_one("SELECT current_user", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        frontegg_user
    );

    // Disable giving out more refresh tokens.
    frontegg_server
        .enable_refresh
        .store(false, Ordering::Relaxed);
    frontegg_server.wait_for_refresh(EXPIRES_IN_SECS);
    // Sleep until the expiry future should resolve.
    tokio::time::sleep(Duration::from_secs(EXPIRES_IN_SECS + 1)).await;
    assert!(pg_client
        .query_one("SELECT current_user", &[])
        .await
        .is_err());
}

#[allow(clippy::unit_arg)]
#[mz_ore::test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `OPENSSL_init_ssl` on OS `linux`
async fn test_auth_base_require_tls_frontegg() {
    let ca = Ca::new_root("test ca").unwrap();
    let (server_cert, server_key) = ca
        .request_cert("server", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .unwrap();
    let metrics_registry = MetricsRegistry::new();

    let tenant_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let secret = Uuid::new_v4();
    let system_client_id = Uuid::new_v4();
    let system_secret = Uuid::new_v4();
    let users = BTreeMap::from([
        (
            (client_id.to_string(), secret.to_string()),
            "uSeR@_.com".to_string(),
        ),
        (
            (system_client_id.to_string(), system_secret.to_string()),
            SYSTEM_USER.name.to_string(),
        ),
    ]);
    let roles = BTreeMap::from([
        ("uSeR@_.com".to_string(), Vec::new()),
        (SYSTEM_USER.name.to_string(), Vec::new()),
    ]);
    let encoding_key =
        EncodingKey::from_rsa_pem(&ca.pkey.private_key_to_pem_pkcs8().unwrap()).unwrap();
    let timestamp = Arc::new(Mutex::new(500_000));
    let now = {
        let timestamp = Arc::clone(&timestamp);
        NowFn::from(move || *timestamp.lock().unwrap())
    };
    let claims = Claims {
        exp: 1000,
        email: "uSeR@_.com".to_string(),
        sub: Uuid::new_v4(),
        user_id: None,
        tenant_id,
        roles: Vec::new(),
        permissions: Vec::new(),
    };
    let frontegg_jwt = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &encoding_key,
    )
    .unwrap();
    let bad_tenant_claims = {
        let mut claims = claims.clone();
        claims.tenant_id = Uuid::new_v4();
        claims
    };
    let bad_tenant_jwt = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &bad_tenant_claims,
        &encoding_key,
    )
    .unwrap();
    let expired_claims = {
        let mut claims = claims;
        claims.exp = 0;
        claims
    };
    let expired_jwt = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &expired_claims,
        &encoding_key,
    )
    .unwrap();
    let frontegg_server = FronteggMockServer::start(
        None,
        encoding_key,
        tenant_id,
        users,
        roles,
        now.clone(),
        1_000,
        None,
    )
    .unwrap();

    let frontegg_auth = FronteggAuthentication::new(
        FronteggConfig {
            admin_api_token_url: frontegg_server.url,
            decoding_key: DecodingKey::from_rsa_pem(&ca.pkey.public_key_to_pem().unwrap()).unwrap(),
            tenant_id: Some(tenant_id),
            now,
            admin_role: "mzadmin".to_string(),
        },
        mz_frontegg_auth::Client::default(),
        &metrics_registry,
    );
    let frontegg_user = "uSeR@_.com";
    let frontegg_password = &format!("mzp_{client_id}{secret}");
    let frontegg_basic = Authorization::basic(frontegg_user, frontegg_password);
    let frontegg_header_basic = make_header(frontegg_basic);

    let frontegg_user_lowercase = frontegg_user.to_lowercase();
    let frontegg_basic_lowercase =
        Authorization::basic(&frontegg_user_lowercase, frontegg_password);
    let frontegg_header_basic_lowercase = make_header(frontegg_basic_lowercase);

    let frontegg_system_password = &format!("mzp_{system_client_id}{system_secret}");
    let frontegg_system_basic = Authorization::basic(&SYSTEM_USER.name, frontegg_system_password);
    let frontegg_system_header_basic = make_header(frontegg_system_basic);

    let no_headers = HeaderMap::new();

    // Test connecting to a server that requires TLS and uses Materialize Cloud for
    // authentication.
    let server = test_util::TestHarness::default()
        .with_tls(server_cert, server_key)
        .with_frontegg(&frontegg_auth)
        .with_metrics_registry(metrics_registry)
        .start()
        .await;

    run_tests(
        "TlsMode::Require, MzCloud",
        &server,
        &[
            TestCase::Ws {
                auth: &WebSocketAuth::Basic {
                    user: frontegg_user.to_string(),
                    password: frontegg_password.to_string(),
                    options: BTreeMap::default(),
                },
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            TestCase::Ws {
                auth: &WebSocketAuth::Bearer {
                    token: frontegg_jwt.clone(),
                    options: BTreeMap::default(),
                },
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            TestCase::Ws {
                auth: &WebSocketAuth::Basic {
                    user: "bad user".to_string(),
                    password: frontegg_password.to_string(),
                    options: BTreeMap::default(),
                },
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|code, message| {
                    assert_eq!(code, CloseCode::Protocol);
                    assert_eq!(message, "unauthorized");
                })),
            },
            // TLS with a password should succeed.
            TestCase::Pgwire {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                password: Some(frontegg_password),
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            TestCase::Http {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                scheme: Scheme::HTTPS,
                headers: &frontegg_header_basic,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            // Email comparisons should be case insensitive.
            TestCase::Pgwire {
                user_to_auth_as: &frontegg_user_lowercase,
                user_reported_by_system: frontegg_user,
                password: Some(frontegg_password),
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            TestCase::Http {
                user_to_auth_as: &frontegg_user_lowercase,
                user_reported_by_system: frontegg_user,
                scheme: Scheme::HTTPS,
                headers: &frontegg_header_basic_lowercase,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            TestCase::Ws {
                auth: &WebSocketAuth::Basic {
                    user: frontegg_user_lowercase.to_string(),
                    password: frontegg_password.to_string(),
                    options: BTreeMap::default(),
                },
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            // Password can be base64 encoded UUID bytes.
            TestCase::Pgwire {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                password: {
                    let mut buf = vec![];
                    buf.extend(client_id.as_bytes());
                    buf.extend(secret.as_bytes());
                    Some(&format!(
                        "mzp_{}",
                        base64::encode_config(buf, base64::URL_SAFE)
                    ))
                },
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            // Password can be base64 encoded UUID bytes without padding.
            TestCase::Pgwire {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                password: {
                    let mut buf = vec![];
                    buf.extend(client_id.as_bytes());
                    buf.extend(secret.as_bytes());
                    Some(&format!(
                        "mzp_{}",
                        base64::encode_config(buf, base64::URL_SAFE_NO_PAD)
                    ))
                },
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            // Password can include arbitrary special characters.
            TestCase::Pgwire {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                password: {
                    let mut password = frontegg_password.clone();
                    password.insert(10, '-');
                    password.insert_str(15, "@#!");
                    Some(&password.clone())
                },
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            // Bearer auth doesn't need the clientid or secret.
            TestCase::Http {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                scheme: Scheme::HTTPS,
                headers: &make_header(Authorization::bearer(&frontegg_jwt).unwrap()),
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            // No TLS fails.
            TestCase::Pgwire {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                password: Some(frontegg_password),
                ssl_mode: SslMode::Disable,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::DbErr(Box::new(|err| {
                    assert_eq!(
                        *err.code(),
                        SqlState::SQLSERVER_REJECTED_ESTABLISHMENT_OF_SQLCONNECTION
                    );
                    assert_eq!(err.message(), "TLS encryption is required");
                })),
            },
            TestCase::Http {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                scheme: Scheme::HTTP,
                headers: &frontegg_header_basic,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: assert_http_rejected(),
            },
            // Wrong, but existing, username.
            TestCase::Pgwire {
                user_to_auth_as: "materialize",
                user_reported_by_system: "materialize",
                password: Some(frontegg_password),
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::DbErr(Box::new(|err| {
                    assert_eq!(err.message(), "invalid password");
                    assert_eq!(*err.code(), SqlState::INVALID_PASSWORD);
                })),
            },
            TestCase::Http {
                user_to_auth_as: "materialize",
                user_reported_by_system: "materialize",
                scheme: Scheme::HTTPS,
                headers: &make_header(Authorization::basic("materialize", frontegg_password)),
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|code, message| {
                    assert_eq!(code, Some(StatusCode::UNAUTHORIZED));
                    assert_eq!(message, "unauthorized");
                })),
            },
            // Wrong password.
            TestCase::Pgwire {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                password: Some("bad password"),
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::DbErr(Box::new(|err| {
                    assert_eq!(err.message(), "invalid password");
                    assert_eq!(*err.code(), SqlState::INVALID_PASSWORD);
                })),
            },
            TestCase::Http {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                scheme: Scheme::HTTPS,
                headers: &make_header(Authorization::basic(frontegg_user, "bad password")),
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|code, message| {
                    assert_eq!(code, Some(StatusCode::UNAUTHORIZED));
                    assert_eq!(message, "unauthorized");
                })),
            },
            // Bad password prefix.
            TestCase::Pgwire {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                password: Some(&format!("mznope_{client_id}{secret}")),
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::DbErr(Box::new(|err| {
                    assert_eq!(err.message(), "invalid password");
                    assert_eq!(*err.code(), SqlState::INVALID_PASSWORD);
                })),
            },
            TestCase::Http {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                scheme: Scheme::HTTPS,
                headers: &make_header(Authorization::basic(
                    frontegg_user,
                    &format!("mznope_{client_id}{secret}"),
                )),
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|code, message| {
                    assert_eq!(code, Some(StatusCode::UNAUTHORIZED));
                    assert_eq!(message, "unauthorized");
                })),
            },
            // No password.
            TestCase::Pgwire {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                password: None,
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::DbErr(Box::new(|err| {
                    assert_eq!(err.message(), "invalid password");
                    assert_eq!(*err.code(), SqlState::INVALID_PASSWORD);
                })),
            },
            TestCase::Http {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                scheme: Scheme::HTTPS,
                headers: &no_headers,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|code, message| {
                    assert_eq!(code, Some(StatusCode::UNAUTHORIZED));
                    assert_eq!(message, "unauthorized");
                })),
            },
            // Bad auth scheme
            TestCase::Http {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                scheme: Scheme::HTTPS,
                headers: &HeaderMap::from_iter(vec![(
                    AUTHORIZATION,
                    HeaderValue::from_static("Digest username=materialize"),
                )]),
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|code, message| {
                    assert_eq!(code, Some(StatusCode::UNAUTHORIZED));
                    assert_eq!(message, "unauthorized");
                })),
            },
            // Bad tenant.
            TestCase::Http {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                scheme: Scheme::HTTPS,
                headers: &make_header(Authorization::bearer(&bad_tenant_jwt).unwrap()),
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|code, message| {
                    assert_eq!(code, Some(StatusCode::UNAUTHORIZED));
                    assert_eq!(message, "unauthorized");
                })),
            },
            // Expired.
            TestCase::Http {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                scheme: Scheme::HTTPS,
                headers: &make_header(Authorization::bearer(&expired_jwt).unwrap()),
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|code, message| {
                    assert_eq!(code, Some(StatusCode::UNAUTHORIZED));
                    assert_eq!(message, "unauthorized");
                })),
            },
            // System user cannot login via external ports.
            TestCase::Pgwire {
                user_to_auth_as: &*SYSTEM_USER.name,
                user_reported_by_system: &*SYSTEM_USER.name,
                password: Some(frontegg_system_password),
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|err| {
                    assert_contains!(err.to_string(), "unauthorized login to user 'mz_system'");
                })),
            },
            TestCase::Http {
                user_to_auth_as: &*SYSTEM_USER.name,
                user_reported_by_system: &*SYSTEM_USER.name,
                scheme: Scheme::HTTPS,
                headers: &frontegg_system_header_basic,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|code, message| {
                    assert_eq!(code, Some(StatusCode::UNAUTHORIZED));
                    assert_contains!(message, "unauthorized");
                })),
            },
            TestCase::Ws {
                auth: &WebSocketAuth::Basic {
                    user: (&*SYSTEM_USER.name).into(),
                    password: frontegg_system_password.to_string(),
                    options: BTreeMap::default(),
                },
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|code, message| {
                    assert_eq!(code, CloseCode::Protocol);
                    assert_eq!(message, "unauthorized");
                })),
            },
            // Public role cannot login.
            TestCase::Pgwire {
                user_to_auth_as: PUBLIC_ROLE_NAME.as_str(),
                user_reported_by_system: PUBLIC_ROLE_NAME.as_str(),
                password: Some(frontegg_system_password),
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|err| {
                    assert_contains!(err.to_string(), "unauthorized login to user 'PUBLIC'");
                })),
            },
            TestCase::Http {
                user_to_auth_as: PUBLIC_ROLE_NAME.as_str(),
                user_reported_by_system: PUBLIC_ROLE_NAME.as_str(),
                scheme: Scheme::HTTPS,
                headers: &frontegg_system_header_basic,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|code, message| {
                    assert_eq!(code, Some(StatusCode::UNAUTHORIZED));
                    assert_contains!(message, "unauthorized");
                })),
            },
            TestCase::Ws {
                auth: &WebSocketAuth::Basic {
                    user: (PUBLIC_ROLE_NAME.as_str()).into(),
                    password: frontegg_system_password.to_string(),
                    options: BTreeMap::default(),
                },
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Err(Box::new(|code, message| {
                    assert_eq!(code, CloseCode::Protocol);
                    assert_eq!(message, "unauthorized");
                })),
            },
        ],
    )
    .await;
}

#[allow(clippy::unit_arg)]
#[mz_ore::test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `OPENSSL_init_ssl` on OS `linux`
async fn test_auth_base_disable_tls() {
    let no_headers = HeaderMap::new();

    // Test TLS modes with a server that does not support TLS.
    let server = test_util::TestHarness::default().start().await;
    run_tests(
        "TlsMode::Disable",
        &server,
        &[
            // Explicitly disabling TLS should succeed.
            TestCase::Pgwire {
                user_to_auth_as: "materialize",
                user_reported_by_system: "materialize",
                password: None,
                ssl_mode: SslMode::Disable,
                configure: Box::new(|_| Ok(())),
                assert: Assert::Success,
            },
            TestCase::Http {
                user_to_auth_as: &*HTTP_DEFAULT_USER.name,
                user_reported_by_system: &*HTTP_DEFAULT_USER.name,
                scheme: Scheme::HTTP,
                headers: &no_headers,
                configure: Box::new(|_| Ok(())),
                assert: Assert::Success,
            },
            // Preferring TLS should fall back to no TLS.
            TestCase::Pgwire {
                user_to_auth_as: "materialize",
                user_reported_by_system: "materialize",
                password: None,
                ssl_mode: SslMode::Prefer,
                configure: Box::new(|_| Ok(())),
                assert: Assert::Success,
            },
            // Requiring TLS should fail.
            TestCase::Pgwire {
                user_to_auth_as: "materialize",
                user_reported_by_system: "materialize",
                password: None,
                ssl_mode: SslMode::Require,
                configure: Box::new(|_| Ok(())),
                assert: Assert::Err(Box::new(|err| {
                    assert_eq!(
                        err.to_string(),
                        "error performing TLS handshake: server does not support TLS",
                    )
                })),
            },
            TestCase::Http {
                user_to_auth_as: &*HTTP_DEFAULT_USER.name,
                user_reported_by_system: &*HTTP_DEFAULT_USER.name,
                scheme: Scheme::HTTPS,
                headers: &no_headers,
                configure: Box::new(|_| Ok(())),
                assert: Assert::Err(Box::new(|code, message| {
                    // Connecting to an HTTP server via HTTPS does not yield
                    // a graceful error message. This could plausibly change
                    // due to OpenSSL or Hyper refactorings.
                    assert!(code.is_none());
                    assert_contains!(message, "ssl3_get_record:wrong version number");
                })),
            },
            // System user cannot login via external ports.
            TestCase::Pgwire {
                user_to_auth_as: &*SYSTEM_USER.name,
                user_reported_by_system: &*SYSTEM_USER.name,
                password: None,
                ssl_mode: SslMode::Disable,
                configure: Box::new(|_| Ok(())),
                assert: Assert::DbErr(Box::new(|err| {
                    assert_contains!(err.to_string(), "unauthorized login to user 'mz_system'");
                })),
            },
        ],
    )
    .await;
}

#[allow(clippy::unit_arg)]
#[mz_ore::test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `OPENSSL_init_ssl` on OS `linux`
async fn test_auth_base_require_tls() {
    let ca = Ca::new_root("test ca").unwrap();
    let (server_cert, server_key) = ca
        .request_cert("server", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .unwrap();

    let client_id = Uuid::new_v4();
    let secret = Uuid::new_v4();
    let frontegg_user = "uSeR@_.com";
    let frontegg_password = &format!("mzp_{client_id}{secret}");
    let frontegg_basic = Authorization::basic(frontegg_user, frontegg_password);
    let frontegg_header_basic = make_header(frontegg_basic);

    let no_headers = HeaderMap::new();

    // Test TLS modes with a server that requires TLS.
    let server = test_util::TestHarness::default()
        .with_tls(server_cert, server_key)
        .start()
        .await;

    run_tests(
        "TlsMode::Require",
        &server,
        &[
            // Non-existent role will be created.
            TestCase::Pgwire {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                password: Some(frontegg_password),
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            // Test that specifying an mzcloud header does nothing and uses the default
            // user.
            TestCase::Http {
                user_to_auth_as: &*HTTP_DEFAULT_USER.name,
                user_reported_by_system: &*HTTP_DEFAULT_USER.name,
                scheme: Scheme::HTTPS,
                headers: &frontegg_header_basic,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            // Disabling TLS should fail.
            TestCase::Pgwire {
                user_to_auth_as: "materialize",
                user_reported_by_system: "materialize",
                password: None,
                ssl_mode: SslMode::Disable,
                configure: Box::new(|_| Ok(())),
                assert: Assert::DbErr(Box::new(|err| {
                    assert_eq!(
                        *err.code(),
                        SqlState::SQLSERVER_REJECTED_ESTABLISHMENT_OF_SQLCONNECTION
                    );
                    assert_eq!(err.message(), "TLS encryption is required");
                })),
            },
            TestCase::Http {
                user_to_auth_as: &*HTTP_DEFAULT_USER.name,
                user_reported_by_system: &*HTTP_DEFAULT_USER.name,
                scheme: Scheme::HTTP,
                headers: &no_headers,
                configure: Box::new(|_| Ok(())),
                assert: assert_http_rejected(),
            },
            // Preferring TLS should succeed.
            TestCase::Pgwire {
                user_to_auth_as: "materialize",
                user_reported_by_system: "materialize",
                password: None,
                ssl_mode: SslMode::Prefer,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            // Requiring TLS should succeed.
            TestCase::Pgwire {
                user_to_auth_as: "materialize",
                user_reported_by_system: "materialize",
                password: None,
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            TestCase::Http {
                user_to_auth_as: &*HTTP_DEFAULT_USER.name,
                user_reported_by_system: &*HTTP_DEFAULT_USER.name,
                scheme: Scheme::HTTPS,
                headers: &no_headers,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::Success,
            },
            // System user cannot login via external ports.
            TestCase::Pgwire {
                user_to_auth_as: &*SYSTEM_USER.name,
                user_reported_by_system: &*SYSTEM_USER.name,
                password: None,
                ssl_mode: SslMode::Prefer,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::DbErr(Box::new(|err| {
                    assert_contains!(err.to_string(), "unauthorized login to user 'mz_system'");
                })),
            },
        ],
    )
    .await;
}

#[mz_ore::test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `OPENSSL_init_ssl` on OS `linux`
async fn test_auth_intermediate_ca_no_intermediary() {
    // Create a CA, an intermediate CA, and a server key pair signed by the
    // intermediate CA.
    let ca = Ca::new_root("test ca").unwrap();
    let intermediate_ca = ca.request_ca("intermediary").unwrap();
    let (server_cert, server_key) = intermediate_ca
        .request_cert("server", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .unwrap();

    // When the server presents only its own certificate, without the
    // intermediary, the client should fail to verify the chain.
    let server = test_util::TestHarness::default()
        .with_tls(server_cert, server_key)
        .start()
        .await;

    run_tests(
        "TlsMode::Require",
        &server,
        &[
            TestCase::Pgwire {
                user_to_auth_as: "materialize",
                user_reported_by_system: "materialize",
                password: None,
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| b.set_ca_file(ca.ca_cert_path())),
                assert: Assert::Err(Box::new(|err| {
                    assert_contains!(err.to_string(), "unable to get local issuer certificate");
                })),
            },
            TestCase::Http {
                user_to_auth_as: &*HTTP_DEFAULT_USER.name,
                user_reported_by_system: &*HTTP_DEFAULT_USER.name,
                scheme: Scheme::HTTPS,
                headers: &HeaderMap::new(),
                configure: Box::new(|b| b.set_ca_file(ca.ca_cert_path())),
                assert: Assert::Err(Box::new(|code, message| {
                    assert!(code.is_none());
                    assert_contains!(message, "unable to get local issuer certificate");
                })),
            },
        ],
    )
    .await;
}

#[mz_ore::test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `OPENSSL_init_ssl` on OS `linux`
async fn test_auth_intermediate_ca() {
    // Create a CA, an intermediate CA, and a server key pair signed by the
    // intermediate CA.
    let ca = Ca::new_root("test ca").unwrap();
    let intermediate_ca = ca.request_ca("intermediary").unwrap();
    let (server_cert, server_key) = intermediate_ca
        .request_cert("server", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .unwrap();

    // Create a certificate chain bundle that contains the server's certificate
    // and the intermediate CA's certificate.
    let server_cert_chain = {
        let path = intermediate_ca.dir.path().join("server.chain.crt");
        let mut buf = vec![];
        File::open(server_cert)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        File::open(intermediate_ca.ca_cert_path())
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        fs::write(&path, buf).unwrap();
        path
    };

    // When the server is configured to present the entire certificate chain,
    // the client should be able to verify the chain even though it only knows
    // about the root CA.
    let server = test_util::TestHarness::default()
        .with_tls(server_cert_chain, server_key)
        .start()
        .await;

    run_tests(
        "TlsMode::Require",
        &server,
        &[
            TestCase::Pgwire {
                user_to_auth_as: "materialize",
                user_reported_by_system: "materialize",
                password: None,
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| b.set_ca_file(ca.ca_cert_path())),
                assert: Assert::Success,
            },
            TestCase::Http {
                user_to_auth_as: &*HTTP_DEFAULT_USER.name,
                user_reported_by_system: &*HTTP_DEFAULT_USER.name,
                scheme: Scheme::HTTPS,
                headers: &HeaderMap::new(),
                configure: Box::new(|b| b.set_ca_file(ca.ca_cert_path())),
                assert: Assert::Success,
            },
        ],
    )
    .await;
}

#[mz_ore::test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `OPENSSL_init_ssl` on OS `linux`
async fn test_auth_admin_non_superuser() {
    let ca = Ca::new_root("test ca").unwrap();
    let (server_cert, server_key) = ca
        .request_cert("server", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .unwrap();
    let metrics_registry = MetricsRegistry::new();

    let tenant_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let secret = Uuid::new_v4();
    let admin_client_id = Uuid::new_v4();
    let admin_secret = Uuid::new_v4();

    let frontegg_user = "user@_.com";
    let admin_frontegg_user = "admin@_.com";

    let admin_role = "mzadmin";

    let users = BTreeMap::from([
        (
            (client_id.to_string(), secret.to_string()),
            frontegg_user.to_string(),
        ),
        (
            (admin_client_id.to_string(), admin_secret.to_string()),
            admin_frontegg_user.to_string(),
        ),
    ]);
    let roles = BTreeMap::from([
        (frontegg_user.to_string(), Vec::new()),
        (
            admin_frontegg_user.to_string(),
            vec![admin_role.to_string()],
        ),
    ]);
    let encoding_key =
        EncodingKey::from_rsa_pem(&ca.pkey.private_key_to_pem_pkcs8().unwrap()).unwrap();
    let now = SYSTEM_TIME.clone();

    let frontegg_server = FronteggMockServer::start(
        None,
        encoding_key,
        tenant_id,
        users,
        roles,
        now.clone(),
        i64::try_from(EXPIRES_IN_SECS).unwrap(),
        None,
    )
    .unwrap();

    let password_prefix = "mzp_";
    let frontegg_auth = FronteggAuthentication::new(
        FronteggConfig {
            admin_api_token_url: frontegg_server.url.clone(),
            decoding_key: DecodingKey::from_rsa_pem(&ca.pkey.public_key_to_pem().unwrap()).unwrap(),
            tenant_id: Some(tenant_id),
            now,
            admin_role: admin_role.to_string(),
        },
        mz_frontegg_auth::Client::default(),
        &metrics_registry,
    );

    let frontegg_password = &format!("{password_prefix}{client_id}{secret}");
    let frontegg_basic = Authorization::basic(frontegg_user, frontegg_password);
    let frontegg_header_basic = make_header(frontegg_basic);

    let server = test_util::TestHarness::default()
        .with_tls(server_cert, server_key)
        .with_frontegg(&frontegg_auth)
        .with_metrics_registry(metrics_registry)
        .start()
        .await;

    run_tests(
        "Non-superuser",
        &server,
        &[
            TestCase::Pgwire {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                password: Some(frontegg_password),
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::SuccessSuperuserCheck(false),
            },
            TestCase::Http {
                user_to_auth_as: frontegg_user,
                user_reported_by_system: frontegg_user,
                scheme: Scheme::HTTPS,
                headers: &frontegg_header_basic,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::SuccessSuperuserCheck(false),
            },
            TestCase::Ws {
                auth: &WebSocketAuth::Basic {
                    user: frontegg_user.to_string(),
                    password: frontegg_password.to_string(),
                    options: BTreeMap::default(),
                },
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::SuccessSuperuserCheck(false),
            },
        ],
    )
    .await;
}

#[mz_ore::test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `OPENSSL_init_ssl` on OS `linux`
async fn test_auth_admin_superuser() {
    let ca = Ca::new_root("test ca").unwrap();
    let (server_cert, server_key) = ca
        .request_cert("server", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .unwrap();
    let metrics_registry = MetricsRegistry::new();

    let tenant_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let secret = Uuid::new_v4();
    let admin_client_id = Uuid::new_v4();
    let admin_secret = Uuid::new_v4();

    let frontegg_user = "user@_.com";
    let admin_frontegg_user = "admin@_.com";

    let admin_role = "mzadmin";

    let users = BTreeMap::from([
        (
            (client_id.to_string(), secret.to_string()),
            frontegg_user.to_string(),
        ),
        (
            (admin_client_id.to_string(), admin_secret.to_string()),
            admin_frontegg_user.to_string(),
        ),
    ]);
    let roles = BTreeMap::from([
        (frontegg_user.to_string(), Vec::new()),
        (
            admin_frontegg_user.to_string(),
            vec![admin_role.to_string()],
        ),
    ]);
    let encoding_key =
        EncodingKey::from_rsa_pem(&ca.pkey.private_key_to_pem_pkcs8().unwrap()).unwrap();
    let now = SYSTEM_TIME.clone();

    let frontegg_server = FronteggMockServer::start(
        None,
        encoding_key,
        tenant_id,
        users,
        roles,
        now.clone(),
        i64::try_from(EXPIRES_IN_SECS).unwrap(),
        None,
    )
    .unwrap();

    let password_prefix = "mzp_";
    let frontegg_auth = FronteggAuthentication::new(
        FronteggConfig {
            admin_api_token_url: frontegg_server.url.clone(),
            decoding_key: DecodingKey::from_rsa_pem(&ca.pkey.public_key_to_pem().unwrap()).unwrap(),
            tenant_id: Some(tenant_id),
            now,
            admin_role: admin_role.to_string(),
        },
        mz_frontegg_auth::Client::default(),
        &metrics_registry,
    );

    let admin_frontegg_password = &format!("{password_prefix}{admin_client_id}{admin_secret}");
    let admin_frontegg_basic = Authorization::basic(admin_frontegg_user, admin_frontegg_password);
    let admin_frontegg_header_basic = make_header(admin_frontegg_basic);

    let server = test_util::TestHarness::default()
        .with_tls(server_cert, server_key)
        .with_frontegg(&frontegg_auth)
        .with_metrics_registry(metrics_registry)
        .start()
        .await;

    run_tests(
        "Superuser",
        &server,
        &[
            TestCase::Pgwire {
                user_to_auth_as: admin_frontegg_user,
                user_reported_by_system: admin_frontegg_user,
                password: Some(admin_frontegg_password),
                ssl_mode: SslMode::Require,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::SuccessSuperuserCheck(true),
            },
            TestCase::Http {
                user_to_auth_as: admin_frontegg_user,
                user_reported_by_system: admin_frontegg_user,
                scheme: Scheme::HTTPS,
                headers: &admin_frontegg_header_basic,
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::SuccessSuperuserCheck(true),
            },
            TestCase::Ws {
                auth: &WebSocketAuth::Basic {
                    user: admin_frontegg_user.to_string(),
                    password: admin_frontegg_password.to_string(),
                    options: BTreeMap::default(),
                },
                configure: Box::new(|b| Ok(b.set_verify(SslVerifyMode::NONE))),
                assert: Assert::SuccessSuperuserCheck(true),
            },
        ],
    )
    .await;
}

#[mz_ore::test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `OPENSSL_init_ssl` on OS `linux`
async fn test_auth_admin_superuser_revoked() {
    let ca = Ca::new_root("test ca").unwrap();
    let (server_cert, server_key) = ca
        .request_cert("server", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .unwrap();
    let metrics_registry = MetricsRegistry::new();

    let tenant_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let secret = Uuid::new_v4();
    let admin_client_id = Uuid::new_v4();
    let admin_secret = Uuid::new_v4();

    let frontegg_user = "user@_.com";
    let admin_frontegg_user = "admin@_.com";

    let admin_role = "mzadmin";

    let users = BTreeMap::from([
        (
            (client_id.to_string(), secret.to_string()),
            frontegg_user.to_string(),
        ),
        (
            (admin_client_id.to_string(), admin_secret.to_string()),
            admin_frontegg_user.to_string(),
        ),
    ]);
    let roles = BTreeMap::from([
        (frontegg_user.to_string(), Vec::new()),
        (
            admin_frontegg_user.to_string(),
            vec![admin_role.to_string()],
        ),
    ]);
    let encoding_key =
        EncodingKey::from_rsa_pem(&ca.pkey.private_key_to_pem_pkcs8().unwrap()).unwrap();
    let now = SYSTEM_TIME.clone();

    let frontegg_server = FronteggMockServer::start(
        None,
        encoding_key,
        tenant_id,
        users,
        roles,
        now.clone(),
        i64::try_from(EXPIRES_IN_SECS).unwrap(),
        None,
    )
    .unwrap();

    let password_prefix = "mzp_";
    let frontegg_auth = FronteggAuthentication::new(
        FronteggConfig {
            admin_api_token_url: frontegg_server.url.clone(),
            decoding_key: DecodingKey::from_rsa_pem(&ca.pkey.public_key_to_pem().unwrap()).unwrap(),
            tenant_id: Some(tenant_id),
            now,
            admin_role: admin_role.to_string(),
        },
        mz_frontegg_auth::Client::default(),
        &metrics_registry,
    );

    let frontegg_password = &format!("{password_prefix}{client_id}{secret}");

    let server = test_util::TestHarness::default()
        .with_tls(server_cert, server_key)
        .with_frontegg(&frontegg_auth)
        .with_metrics_registry(metrics_registry)
        .start()
        .await;

    let pg_client = server
        .connect()
        .ssl_mode(SslMode::Require)
        .user(frontegg_user)
        .password(frontegg_password)
        .with_tls(make_pg_tls(Box::new(|b: &mut SslConnectorBuilder| {
            Ok(b.set_verify(SslVerifyMode::NONE))
        })))
        .await
        .unwrap();

    assert_eq!(
        pg_client
            .query_one("SHOW is_superuser", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "off"
    );

    frontegg_server
        .role_updates_tx
        .send((frontegg_user.to_string(), vec![admin_role.to_string()]))
        .unwrap();
    frontegg_server.wait_for_refresh(EXPIRES_IN_SECS);

    assert_eq!(
        pg_client
            .query_one("SHOW is_superuser", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "on"
    );

    frontegg_server
        .role_updates_tx
        .send((frontegg_user.to_string(), Vec::new()))
        .unwrap();
    frontegg_server.wait_for_refresh(EXPIRES_IN_SECS);

    assert_eq!(
        pg_client
            .query_one("SHOW is_superuser", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "off"
    );
}

#[mz_ore::test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `OPENSSL_init_ssl` on OS `linux`
async fn test_auth_deduplication() {
    let ca = Ca::new_root("test ca").unwrap();
    let (server_cert, server_key) = ca
        .request_cert("server", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .unwrap();
    let metrics_registry = MetricsRegistry::new();

    let tenant_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let secret = Uuid::new_v4();
    let users = BTreeMap::from([(
        (client_id.to_string(), secret.to_string()),
        "user@_.com".to_string(),
    )]);
    let roles = BTreeMap::from([("user@_.com".to_string(), Vec::new())]);
    let encoding_key =
        EncodingKey::from_rsa_pem(&ca.pkey.private_key_to_pem_pkcs8().unwrap()).unwrap();

    let frontegg_server = FronteggMockServer::start(
        None,
        encoding_key,
        tenant_id,
        users,
        roles,
        SYSTEM_TIME.clone(),
        i64::try_from(EXPIRES_IN_SECS).unwrap(),
        Some(Duration::from_secs(2)),
    )
    .unwrap();

    let frontegg_auth = FronteggAuthentication::new(
        FronteggConfig {
            admin_api_token_url: frontegg_server.url.clone(),
            decoding_key: DecodingKey::from_rsa_pem(&ca.pkey.public_key_to_pem().unwrap()).unwrap(),
            tenant_id: Some(tenant_id),
            now: SYSTEM_TIME.clone(),
            admin_role: "mzadmin".to_string(),
        },
        mz_frontegg_auth::Client::default(),
        &metrics_registry,
    );
    let frontegg_user = "user@_.com";
    let frontegg_password = &format!("mzp_{client_id}{secret}");

    let server = test_util::TestHarness::default()
        .with_tls(server_cert, server_key)
        .with_frontegg(&frontegg_auth)
        .with_metrics_registry(metrics_registry)
        .start()
        .await;

    assert_eq!(*frontegg_server.auth_requests.lock().unwrap(), 0);

    let pg_client_1_fut = server
        .connect()
        .ssl_mode(SslMode::Require)
        .user(frontegg_user)
        .password(frontegg_password)
        .with_tls(make_pg_tls(Box::new(|b: &mut SslConnectorBuilder| {
            Ok(b.set_verify(SslVerifyMode::NONE))
        })))
        .into_future();

    let pg_client_2_fut = server
        .connect()
        .ssl_mode(SslMode::Require)
        .user(frontegg_user)
        .password(frontegg_password)
        .with_tls(make_pg_tls(Box::new(|b: &mut SslConnectorBuilder| {
            Ok(b.set_verify(SslVerifyMode::NONE))
        })))
        .into_future();

    let (client_1_result, client_2_result) =
        futures::future::join(pg_client_1_fut, pg_client_2_fut).await;
    let pg_client_1 = client_1_result.unwrap();
    let pg_client_2 = client_2_result.unwrap();

    let frontegg_user_client_1 = pg_client_1
        .query_one("SELECT current_user", &[])
        .await
        .unwrap()
        .get::<_, String>(0);
    assert_eq!(frontegg_user_client_1, frontegg_user);

    let frontegg_user_client_2 = pg_client_2
        .query_one("SELECT current_user", &[])
        .await
        .unwrap()
        .get::<_, String>(0);
    assert_eq!(frontegg_user_client_2, frontegg_user);

    // We should have de-duplicated the request and only actually sent 1.
    assert_eq!(*frontegg_server.auth_requests.lock().unwrap(), 1);

    // Wait for a refresh to occur.
    frontegg_server.wait_for_refresh(10);
    assert_eq!(*frontegg_server.refreshes.lock().unwrap(), 1);

    // Both clients should still be queryable.
    let frontegg_user_client_1_post_refresh = pg_client_1
        .query_one("SELECT current_user", &[])
        .await
        .unwrap()
        .get::<_, String>(0);
    assert_eq!(frontegg_user_client_1_post_refresh, frontegg_user);

    let frontegg_user_client_2_post_refresh = pg_client_2
        .query_one("SELECT current_user", &[])
        .await
        .unwrap()
        .get::<_, String>(0);
    assert_eq!(frontegg_user_client_2_post_refresh, frontegg_user);
}

#[mz_ore::test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `OPENSSL_init_ssl` on OS `linux`
async fn test_refresh_task_metrics() {
    let ca = Ca::new_root("test ca").unwrap();
    let (server_cert, server_key) = ca
        .request_cert("server", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .unwrap();
    let metrics_registry = MetricsRegistry::new();

    let tenant_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let secret = Uuid::new_v4();
    let users = BTreeMap::from([(
        (client_id.to_string(), secret.to_string()),
        "user@_.com".to_string(),
    )]);
    let roles = BTreeMap::from([("user@_.com".to_string(), Vec::new())]);
    let encoding_key =
        EncodingKey::from_rsa_pem(&ca.pkey.private_key_to_pem_pkcs8().unwrap()).unwrap();

    let frontegg_server = FronteggMockServer::start(
        None,
        encoding_key,
        tenant_id,
        users,
        roles,
        SYSTEM_TIME.clone(),
        i64::try_from(EXPIRES_IN_SECS).unwrap(),
        None,
    )
    .unwrap();

    let frontegg_auth = FronteggAuthentication::new(
        FronteggConfig {
            admin_api_token_url: frontegg_server.url.clone(),
            decoding_key: DecodingKey::from_rsa_pem(&ca.pkey.public_key_to_pem().unwrap()).unwrap(),
            tenant_id: Some(tenant_id),
            now: SYSTEM_TIME.clone(),
            admin_role: "mzadmin".to_string(),
        },
        mz_frontegg_auth::Client::default(),
        &metrics_registry,
    );
    let frontegg_user = "user@_.com";
    let frontegg_password = &format!("mzp_{client_id}{secret}");

    let server = test_util::TestHarness::default()
        .with_tls(server_cert, server_key)
        .with_frontegg(&frontegg_auth)
        .with_metrics_registry(metrics_registry)
        .start()
        .await;

    let pg_client = server
        .connect()
        .ssl_mode(SslMode::Require)
        .user(frontegg_user)
        .password(frontegg_password)
        .with_tls(make_pg_tls(Box::new(|b: &mut SslConnectorBuilder| {
            Ok(b.set_verify(SslVerifyMode::NONE))
        })))
        .await
        .unwrap();

    assert_eq!(
        pg_client
            .query_one("SELECT current_user", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        frontegg_user
    );

    // Make sure our guage indicates there is one refresh task running.
    let metrics = server.metrics_registry.gather();
    let mut metrics: Vec<_> = metrics
        .into_iter()
        .filter(|family| family.get_name() == "mz_auth_refresh_tasks_active")
        .collect();
    assert_eq!(metrics.len(), 1);
    let metric = metrics.pop().unwrap();
    let metric = &metric.get_metric()[0];
    assert_eq!(metric.get_gauge().get_value(), 1.0);

    drop(pg_client);

    // The refresh task asynchronously notices the client has been dropped, so it might take a
    // moment, hence the retry.
    let result = mz_ore::retry::Retry::default()
        .max_duration(Duration::from_secs(5))
        .retry(|_| {
            // After dropping the client we should not have any refresh tasks running.
            let metrics = server.metrics_registry.gather();
            let mut metrics: Vec<_> = metrics
                .into_iter()
                .filter(|family| family.get_name() == "mz_auth_refresh_tasks_active")
                .collect();

            // If we're retrying, and the metric hasn't changed, then our set will be empty.
            if metrics.len() != 1 {
                return Err(-1.0);
            }

            let metric = metrics.pop().unwrap();
            let metric = &metric.get_metric()[0];

            let guage_value = metric.get_gauge().get_value();
            if guage_value == 0.0 {
                Ok(())
            } else {
                Err(guage_value)
            }
        });

    assert_eq!(result, Ok(()));
}

#[mz_ore::test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `OPENSSL_init_ssl` on OS `linux`
async fn test_superuser_can_alter_cluster() {
    let ca = Ca::new_root("test ca").unwrap();
    let (server_cert, server_key) = ca
        .request_cert("server", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .unwrap();
    let metrics_registry = MetricsRegistry::new();

    let tenant_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let secret = Uuid::new_v4();
    let admin_client_id = Uuid::new_v4();
    let admin_secret = Uuid::new_v4();

    let frontegg_user = "user@_.com";
    let admin_frontegg_user = "admin@_.com";

    let admin_role = "mzadmin";

    let users = BTreeMap::from([
        (
            (client_id.to_string(), secret.to_string()),
            frontegg_user.to_string(),
        ),
        (
            (admin_client_id.to_string(), admin_secret.to_string()),
            admin_frontegg_user.to_string(),
        ),
    ]);
    let roles = BTreeMap::from([
        (frontegg_user.to_string(), Vec::new()),
        (
            admin_frontegg_user.to_string(),
            vec![admin_role.to_string()],
        ),
    ]);
    let encoding_key =
        EncodingKey::from_rsa_pem(&ca.pkey.private_key_to_pem_pkcs8().unwrap()).unwrap();
    let now = SYSTEM_TIME.clone();

    let frontegg_server = FronteggMockServer::start(
        None,
        encoding_key,
        tenant_id,
        users,
        roles,
        now.clone(),
        i64::try_from(EXPIRES_IN_SECS).unwrap(),
        None,
    )
    .unwrap();

    let password_prefix = "mzp_";
    let frontegg_auth = FronteggAuthentication::new(
        FronteggConfig {
            admin_api_token_url: frontegg_server.url.clone(),
            decoding_key: DecodingKey::from_rsa_pem(&ca.pkey.public_key_to_pem().unwrap()).unwrap(),
            tenant_id: Some(tenant_id),
            now,
            admin_role: admin_role.to_string(),
        },
        mz_frontegg_auth::Client::default(),
        &metrics_registry,
    );

    let admin_frontegg_password = format!("{password_prefix}{admin_client_id}{admin_secret}");
    let frontegg_user_password = format!("{password_prefix}{client_id}{secret}");

    let server = test_util::TestHarness::default()
        .with_tls(server_cert, server_key)
        .with_frontegg(&frontegg_auth)
        .with_metrics_registry(metrics_registry)
        .start()
        .await;

    let tls = make_pg_tls(|b| Ok(b.set_verify(SslVerifyMode::NONE)));
    let superuser = server
        .connect()
        .ssl_mode(SslMode::Require)
        .user(admin_frontegg_user)
        .password(&admin_frontegg_password)
        .with_tls(tls.clone())
        .await
        .unwrap();

    let default_cluster = superuser
        .query_one("SHOW cluster", &[])
        .await
        .unwrap()
        .get::<_, String>(0);
    assert_eq!(default_cluster, "quickstart");

    // External admins should be able to modify the system default cluster.
    superuser
        .execute("ALTER SYSTEM SET cluster TO foo_bar", &[])
        .await
        .unwrap();

    // New system defaults only take effect for new sessions.
    let regular_user = server
        .connect()
        .ssl_mode(SslMode::Require)
        .user(frontegg_user)
        .password(&frontegg_user_password)
        .with_tls(tls)
        .await
        .unwrap();

    let new_default_cluster = regular_user
        .query_one("SHOW cluster", &[])
        .await
        .unwrap()
        .get::<_, String>(0);
    assert_eq!(new_default_cluster, "foo_bar");
}
