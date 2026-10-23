//! Class metadata should cross once per class, without aliasing public dictionaries.
#![cfg(feature = "python")]

use allocation_counter::measure;
use monty_proto::python::{DecodedArena, InstanceStore};
use monty_types::{
    MontyUuid,
    unstable::{ClassTypeNode, MontyGraph, MontyNode, NodeId},
};
use pyo3::prelude::*;

/// A class node with one eager attribute and a fixed UUID for identity tests.
fn class(graph: &mut MontyGraph, name: &str, value: i64) -> NodeId {
    let key = graph.push(MontyNode::String("x".to_owned()));
    let value = graph.push(MontyNode::Int(value));
    graph.push(MontyNode::ClassType(Box::new(ClassTypeNode {
        name: name.to_owned(),
        id: MontyUuid::from_u128(0),
        host_defined: false,
        is_dataclass: false,
        attrs: vec![(key, value)],
    })))
}

#[test]
fn long_class_names_are_not_copied_per_instance() {
    let mut graph = MontyGraph::new();
    let class_type = class(&mut graph, &"C".repeat(6000), 42);
    for id in 1..=4000 {
        graph.push(MontyNode::ClassInstance {
            class_type,
            instance_id: MontyUuid::from_u128(id),
            attrs: vec![],
        });
    }
    Python::initialize();
    Python::attach(|py| {
        let store = InstanceStore::new(py);
        let mut decoded = None;
        let allocations = measure(|| {
            decoded = Some(DecodedArena::new(py, &graph, &store).unwrap());
        });
        // The old converter requests over 24 MB of Rust class-name copies.
        // Leave ample headroom for the arena table and small cache bookkeeping.
        assert!(allocations.bytes_total < 1_000_000, "{allocations:?}");
        assert!(decoded.is_some());
    });
}

#[test]
fn distinct_class_nodes_with_the_same_uuid_keep_their_metadata() {
    let mut graph = MontyGraph::new();
    let mut instances = Vec::new();
    for (id, name) in [(1, "First"), (2, "Second")] {
        let class_type = class(&mut graph, name, id);
        instances.push(graph.push(MontyNode::ClassInstance {
            class_type,
            instance_id: MontyUuid::from_u128(u128::try_from(id).unwrap()),
            attrs: vec![],
        }));
    }
    Python::initialize();
    Python::attach(|py| {
        let store = InstanceStore::new(py);
        let decoded = DecodedArena::new(py, &graph, &store).unwrap();
        let names: Vec<String> = instances
            .iter()
            .map(|id| {
                decoded
                    .get(py, *id)
                    .bind(py)
                    .getattr("name")
                    .unwrap()
                    .extract()
                    .unwrap()
            })
            .collect();
        assert_eq!(names, ["First", "Second"]);
    });
}

#[test]
fn class_attribute_dictionaries_are_shared_by_instance_proxies() {
    let mut graph = MontyGraph::new();
    let attrs = (0..128)
        .map(|i| {
            let key = graph.push(MontyNode::String(format!("field_{i}")));
            let value = graph.push(MontyNode::Int(i));
            (key, value)
        })
        .collect();
    let class_type = graph.push(MontyNode::ClassType(Box::new(ClassTypeNode {
        name: "C".to_owned(),
        id: MontyUuid::from_u128(0),
        host_defined: false,
        is_dataclass: false,
        attrs,
    })));
    for id in 1..=4000 {
        graph.push(MontyNode::ClassInstance {
            class_type,
            instance_id: MontyUuid::from_u128(id),
            attrs: vec![],
        });
    }
    Python::initialize();
    Python::attach(|py| {
        let store = InstanceStore::new(py);
        let tracing = py.import("tracemalloc").unwrap();
        tracing.call_method0("start").unwrap();
        let decoded = DecodedArena::new(py, &graph, &store).unwrap();
        let (_, peak): (usize, usize) = tracing.call_method0("get_traced_memory").unwrap().extract().unwrap();
        tracing.call_method0("stop").unwrap();
        // Separate 128-entry dictionaries for 4,000 instances exceed 12 MB.
        // The shared dictionary leaves only each proxy and its own instance attrs.
        assert!(peak < 4 * 1024 * 1024, "Python allocation peak: {peak}");
        assert_eq!(
            decoded
                .get(py, class_type)
                .bind(py)
                .getattr("name")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "C"
        );
    });
}
