use adbc_core::options::{OptionDatabase, OptionValue};
use adbc_core::{Connection, Database, Driver, Statement};
use arrow_array::cast::AsArray;
use arrow_array::types::Decimal128Type;

// Diagnostic, not a regression test: DuckDB's read_adbc() reproducibly returns every
// other row zeroed (rows 2,4,6,... of an N-row result, 1-indexed) once execute_schema()
// is implemented -- confirmed absent when execute_schema() is stubbed back to
// NOT_IMPLEMENTED (git stash the fork's changes and rebuild). Deterministic and
// scale-invariant from 2 to 100,000 rows, tested via ddb-ch-query_duplicate.sh's stack.
//
// This test calls execute_schema() then execute() on the same statement -- the same
// sequence DuckDB's AdbcArrowScanFunctionData constructor + AdbcProduceArrowScan use --
// but it passes cleanly every time: sequential vs. multi-thread tokio runtime
// (ADBC_CLICKHOUSE_TEST_MULTI_THREAD=1), gaps of 0-2000ms between the two calls, and
// DuckDB-side `PRAGMA threads=1` were all ruled out as the trigger. So the corruption
// is introduced somewhere in DuckDB's C++ ADBC extension / FFI boundary, not in this
// crate's execute_schema()/execute() logic in isolation -- worth keeping this test as
// a sanity check that the driver itself is not at fault, and revisiting once the actual
// DuckDB-side mechanism is found.
#[test]
fn diag_execute_schema_then_execute_corrupts_every_other_row() {
    let mut driver = crate::test_driver();
    let db = driver
        .new_database_with_opts([
            (OptionDatabase::Uri, OptionValue::from("http://localhost:8123/")),
            (OptionDatabase::Username, OptionValue::from("duckdb_user")),
            (OptionDatabase::Password, OptionValue::from("duckdb_password")),
        ])
        .unwrap();
    let mut conn = db.new_connection().unwrap();
    let mut statement = conn.new_statement().unwrap();

    let sql = "SELECT l_orderkey, l_linenumber, l_quantity \
               FROM lineitem WHERE l_orderkey = 1 ORDER BY l_linenumber";

    statement.set_sql_query(sql).unwrap();

    let schema = statement.execute_schema().unwrap();
    println!("schema: {schema:?}");

    let reader = statement.execute().unwrap();
    let batches = reader.collect::<Result<Vec<_>, _>>().unwrap();

    for batch in &batches {
        let linenumber = batch.column(1).as_primitive::<arrow_array::types::Int64Type>();
        let quantity = batch.column(2).as_primitive::<Decimal128Type>();
        for i in 0..batch.num_rows() {
            println!("linenumber={} quantity={}", linenumber.value(i), quantity.value(i));
        }
    }
}
