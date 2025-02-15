# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

# Verify behavior of FORMAT JSON

$ kafka-create-topic topic=data partitions=1
$ kafka-ingest format=bytes topic=data
{"a":"b","c":"d"}

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

> CREATE SOURCE data
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-data-${testdrive.seed}')
  FORMAT JSON;

> SELECT DISTINCT pg_typeof(data) FROM data;
jsonb

> SELECT * FROM data
"{\"a\":\"b\",\"c\":\"d\"}"

# It's a dict so this is not just a string masquerading as JSON
> SELECT data -> 'a' FROM data;
"\"b\""

$ kafka-ingest format=bytes topic=data
[1,2,3]
1
1.23
"hello"
""

> SELECT * FROM data
[1,2,3]
1
1.23
"\"hello\""
"\"\""
"{\"a\":\"b\",\"c\":\"d\"}"

$ kafka-ingest format=bytes topic=data
hello

!SELECT * FROM data
exact:Decode error: Bytes: Failed to decode JSON: hello (original bytes: [68, 65, 6c, 6c, 6f])
