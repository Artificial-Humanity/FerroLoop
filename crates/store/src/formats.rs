//! The storage formats this build opens, for `--version --json`.

use serde_json::{Value, json};

/// `{ "redb_schema": {min,max}, "ferrostore_container": {min,max} }`, read from the constants
/// the code opens stores by, never typed here.
///
/// - `redb_schema`: the schema ladder both engines share, oldest to newest this build opens
///   (`FORMAT_VERSION` ..= `FORMAT_WITH_AREA_DELETE`). A new store starts at the oldest and a
///   first write of a newer kind raises it; an older build refuses a raised store.
/// - `ferrostore_container`: the `.fls` container format (`CONTAINER_FORMAT_OLDEST` ..=
///   `CONTAINER_FORMAT_NEWEST`); every slot this build writes is the newest.
pub fn store_formats() -> Value {
    json!({
        "redb_schema": {
            "min": crate::FORMAT_VERSION,
            "max": crate::FORMAT_WITH_AREA_DELETE,
        },
        "ferrostore_container": {
            "min": crate::ferrostore::CONTAINER_FORMAT_OLDEST,
            "max": crate::ferrostore::CONTAINER_FORMAT_NEWEST,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ranges_are_the_ladders_the_code_checks_by() {
        let v = store_formats();
        // The ladder `check_format` accepts starts and ends where the report says.
        assert_eq!(v["redb_schema"]["min"], crate::FORMAT_LADDER[0]);
        assert_eq!(
            v["redb_schema"]["max"],
            crate::FORMAT_LADDER[crate::FORMAT_LADDER.len() - 1]
        );
        assert!(v["redb_schema"]["min"].as_u64() < v["redb_schema"]["max"].as_u64());
        assert!(
            v["ferrostore_container"]["min"].as_u64() <= v["ferrostore_container"]["max"].as_u64()
        );
        assert!(crate::check_format(v["redb_schema"]["max"].as_u64()).is_ok());
        assert!(crate::check_format(Some(v["redb_schema"]["max"].as_u64().unwrap() + 1)).is_err());
    }
}
