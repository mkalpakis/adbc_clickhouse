use adbc_clickhouse::ClickhouseDriver;
use adbc_core::options::OptionDatabase;
use adbc_core::{Connection, Database, Driver, Statement};
use arrow_schema::DataType;

// Demonstrates a real bug in `arrow_decimal()` (`src/schema.rs`) that predates and is unrelated to
// this fork's `execute_schema()` work: `Connection::get_table_schema()` (pre-existing since v0.1.0,
// backed by `schema::of_table()`) reports the wrong Arrow decimal width for any `Decimal(P, S)`
// column with precision in [1, 18] -- but the real data ClickHouse serializes for that exact column
// over `FORMAT ArrowStream` is always `Decimal128` (for P <= 38). See `CORRECTNESS_FINDINGS.md`
// ("BUG 1") and `DECIMAL_ARROW_WIDTH_MISMATCH.md` for the full writeup.
//
// This test uses only `ClickhouseDriver::init()`, `Connection::get_table_schema()`, and
// `Statement::execute()` -- all public API, present since `v0.1.0`/`v0.1.1` -- so it reproduces
// identically against a clean checkout of `upstream/main` (`2c374ad`, tag `v0.1.1`) or against a
// plain `clickhouse-server` container. It's the same code as `DECIMAL_ARROW_WIDTH_MISMATCH.md`'s
// "Code example" -- run against:
//   docker run -d -p 8123:8123 -e CLICKHOUSE_USER=default -e CLICKHOUSE_PASSWORD=password \
//     clickhouse/clickhouse-server
#[test]
fn get_table_schema_decimal_width_disagrees_with_real_data() {
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
    insert
        .set_sql_query("INSERT INTO t VALUES (17.00), (36.00)")
        .unwrap();
    insert.execute_update().unwrap();

    let reported_type = conn
        .get_table_schema(None, None, "t")
        .unwrap()
        .field(0)
        .data_type()
        .clone();

    let mut select = conn.new_statement().unwrap();
    select.set_sql_query("SELECT v FROM t").unwrap();
    let real_type = select
        .execute()
        .unwrap()
        .schema()
        .field(0)
        .data_type()
        .clone();

    eprintln!("get_table_schema() reports: {reported_type:?}");
    eprintln!("real execute() data is:    {real_type:?}");

    assert_eq!(
        reported_type,
        DataType::Decimal64(15, 2),
        "expected get_table_schema() to (incorrectly) report Decimal64 -- \
         if this fails, the upstream bug this test documents may already be fixed"
    );
    assert_eq!(
        real_type,
        DataType::Decimal128(15, 2),
        "expected the real execute() data to be Decimal128, per ClickHouse's own \
         FORMAT ArrowStream serialization rule (see CHColumnToArrowColumn.cpp)"
    );
    assert_ne!(
        reported_type, real_type,
        "BUG: get_table_schema() and execute() disagree on the Arrow type of the same column"
    );
}
