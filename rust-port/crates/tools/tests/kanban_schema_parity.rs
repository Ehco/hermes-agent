//! Byte-frozen schema parity: the Rust `spec_parts` output for all 15 kanban
//! tools must equal the Python agent's `tools/kanban_tools_schemas.py` dump
//! (description + parameters), field for field. The fixture was generated from
//! the Python source of truth with:
//!   python -c "...json.dump({name: {'description': ..., 'parameters': ...}})"
//! and is checked in so the contract is frozen even without a Python install.

use serde_json::Value;
use serde_json::json;

fn fixture() -> Value {
    let path = format!(
        "{}/tests/kanban_schemas_pydump.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(path).expect("python schema dump fixture");
    serde_json::from_str(&raw).expect("fixture is valid json")
}

#[test]
fn rust_schemas_match_the_python_dump_field_for_field() {
    let py = fixture();
    let names = [
        "kanban_show",
        "kanban_list",
        "kanban_complete",
        "kanban_block",
        "kanban_schedule",
        "kanban_request_review",
        "kanban_request_changes",
        "kanban_heartbeat",
        "kanban_comment",
        "kanban_attach",
        "kanban_attach_url",
        "kanban_attachments",
        "kanban_create",
        "kanban_unblock",
        "kanban_link",
    ];
    let specs = hermes_tools::registry::specs();
    for name in names {
        let expected = py
            .get(name)
            .unwrap_or_else(|| panic!("{name} missing from fixture"));
        let spec = specs
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} not registered in the Rust registry"));
        assert_eq!(
            json!({ "description": spec.description, "parameters": spec.schema }),
            *expected,
            "schema drift for {name}"
        );
    }
    // The fixture covers exactly the 15 kanban tools.
    assert_eq!(py.as_object().unwrap().len(), names.len());
}
