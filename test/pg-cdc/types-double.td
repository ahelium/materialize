# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

#
# Test the floating-point types
#

> CREATE SECRET pgpass AS 'postgres'
> CREATE CONNECTION pg TO POSTGRES (
    HOST postgres,
    DATABASE postgres,
    USER postgres,
    PASSWORD SECRET pgpass
  )

# Insert data pre-snapshot
$ postgres-execute connection=postgres://postgres:postgres@postgres
ALTER USER postgres WITH replication;
DROP SCHEMA IF EXISTS public CASCADE;
DROP PUBLICATION IF EXISTS mz_source;

CREATE SCHEMA public;

CREATE TABLE t1 (f1 REAL, f2 DOUBLE PRECISION);
ALTER TABLE t1 REPLICA IDENTITY FULL;

INSERT INTO t1 VALUES (NULL, NULL);
INSERT INTO t1 VALUES ('-Inf', '-Inf')
INSERT INTO t1 VALUES ('NaN', 'NaN');
INSERT INTO t1 VALUES ('+Inf', '+Inf');
INSERT INTO t1 VALUES ('0.000000000000000000000000000001', '0.000000000000000000000000000001');
INSERT INTO t1 VALUES ('-1111111111111111111111111111111', '-1111111111111111111111111111111');

CREATE PUBLICATION mz_source FOR ALL TABLES;

> CREATE SOURCE mz_source
  FROM POSTGRES CONNECTION pg (PUBLICATION 'mz_source')
  FOR ALL TABLES;

> SELECT COUNT(*) > 0 FROM t1;
true

# Insert the same data post-snapshot
$ postgres-execute connection=postgres://postgres:postgres@postgres
INSERT INTO t1 SELECT * FROM t1;

> SELECT pg_typeof(f1), pg_typeof(f2) FROM t1 LIMIT 1;
"real" "double precision"

# Note that there is some expected loss of precision on the very large numbers

> SELECT * FROM t1;
-1111110000000000000000000000000 -1111111111111110000000000000000
-1111110000000000000000000000000 -1111111111111110000000000000000
-inf -inf
-inf -inf
0.000000000000000000000000000001 0.000000000000000000000000000001
0.000000000000000000000000000001 0.000000000000000000000000000001
<null> <null>
<null> <null>
NaN NaN
NaN NaN
inf inf
inf inf
