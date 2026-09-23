### Describe the bug

For columns `Decimal(P, S)` with `1 <= P <= 18`, `Connection::get_table_schema()` reports
`Decimal32`/`Decimal64`, but `Statement::execute()` returns `Decimal128`.

`arrow_decimal()` (`src/schema.rs:379-384`) picks the Arrow decimal width from `Decimal(P, S)`'s
internal storage class (`Decimal32`/`64`/`128`/`256`). But ClickHouse's
`FORMAT ArrowStream` serializer only emits `Decimal128` for precision `<= 38`, and `Decimal256` above that. From ClickHouse `CHColumnToArrowColumn.cpp`
(`master` @ `be9434c2`, L1684-1691):

Clickhouse maintainer comment below explains that this is an intentional choice.

```cpp
const auto precision = decimal_type->getPrecision();
/// Reproduces what the removed `arrow::decimal` did: `Decimal256` above precision 38,
/// `Decimal128` otherwise. `arrow::smallest_decimal` is deliberately not used here -
/// Arrow gained `Decimal32` and `Decimal64`, so it would narrow small decimals and
/// change the schema we write.
arrow_type = precision > 38
    ? arrow::decimal256(precision, decimal_type->getScale())
    : arrow::decimal128(precision, decimal_type->getScale());
```
https://github.com/ClickHouse/ClickHouse/blob/be9434c24112772777fc14021fe1d4703423d495/src/Processors/Formats/Impl/CHColumnToArrowColumn.cpp#L1684-L1691


### Steps to reproduce

1. `CREATE TABLE t (v Decimal(15, 2)) ENGINE = Memory`
2. `INSERT INTO t VALUES (17.00), (36.00)`
3. Call `Connection::get_table_schema(None, None, "t")` and inspect the type of `v`.
4. Call `Statement::execute()` with `SELECT v FROM t`, and inspect type of `v`.

### Expected behaviour

`get_table_schema()` should report `Decimal128(15, 2)` for `v`, matching what `execute()` returns.

Fix: in `arrow_decimal()`, map `Decimal32`/`Decimal64`/`Decimal128` to Arrow `Decimal128`, and leave `Decimal256` alone.

### Code example

```bash
docker run -d -p 8123:8123 -e CLICKHOUSE_USER=default -e CLICKHOUSE_PASSWORD=password \
  clickhouse/clickhouse-server
```

`Cargo.toml`:
```toml
[dependencies]
adbc_clickhouse = "0.1.1"
adbc_core = "0.23"
arrow-schema = "58"
```

`src/main.rs`:
```rust
use adbc_clickhouse::ClickhouseDriver;
use adbc_core::options::OptionDatabase;
use adbc_core::{Connection, Database, Driver, Statement};

fn main() {
    let mut driver = ClickhouseDriver::init();
    let db = driver
        .new_database_with_opts([
            (OptionDatabase::Uri, "http://localhost:8123/".into()),
            (OptionDatabase::Username, "default".into()),
            (OptionDatabase::Password, "password".into()),
        ])
        .unwrap();
    let mut conn = db.new_connection().unwrap();

    let mut create_table = conn.new_statement().unwrap();
    create_table
        .set_sql_query("CREATE OR REPLACE TABLE t (v Decimal(15, 2)) ENGINE = Memory")
        .unwrap();
    create_table.execute_update().unwrap();

    let mut insert = conn.new_statement().unwrap();
    insert.set_sql_query("INSERT INTO t VALUES (17.00), (36.00)").unwrap();
    insert.execute_update().unwrap();

    let reported_type = conn.get_table_schema(None, None, "t").unwrap().field(0).data_type().clone();

    let mut select = conn.new_statement().unwrap();
    select.set_sql_query("SELECT v FROM t").unwrap();
    let real_type = select.execute().unwrap().schema().field(0).data_type().clone();

    println!("get_table_schema() reports: {reported_type:?}");
    println!("real execute() data is:     {real_type:?}");
    assert_eq!(reported_type, real_type); // fails: Decimal64(15, 2) != Decimal128(15, 2)
}
```

| Precision | `arrow_decimal()` currently reports | Actual wire type |
|---|---|---|
| 5, 9   | `Decimal32`  | `Decimal128` |
| 10-18  | `Decimal64`  | `Decimal128` |
| 19-38  | `Decimal128` | `Decimal128` |
| 39-76  | `Decimal256` | `Decimal256` |

### Configuration

#### Environment
* Client version: `adbc_clickhouse` v0.1.1
* OS: macOS (Darwin 24.0.0) and Ubuntu 24.04 (Docker)

#### ClickHouse server
* ClickHouse Server version: 26.8.7.19 (official build)
* ClickHouse Server non-default settings, if any: none
* `CREATE TABLE` statements / sample data: 
```SQL 
CREATE OR REPLACE TABLE t (v Decimal(15, 2)) ENGINE = Memory
```
