use super::*;
use rowboat_core::ForeignKeyColumn;
fn table(schema: &str, name: &str) -> TableRef {
    TableRef {
        schema: schema.into(),
        name: name.into(),
    }
}
fn edge(a: &TableRef, b: &TableRef, name: &str) -> ForeignKeyRelationship {
    ForeignKeyRelationship {
        name: name.into(),
        source: a.clone(),
        referenced: b.clone(),
        columns: vec![
            ForeignKeyColumn {
                source: "tenant".into(),
                referenced: "tenant".into(),
                nullable: false,
            },
            ForeignKeyColumn {
                source: "parent_id".into(),
                referenced: "id".into(),
                nullable: true,
            },
        ],
        on_update: "NO ACTION".into(),
        on_delete: "NO ACTION".into(),
    }
}
#[test]
fn diagram_composite_self_cycle_cross_namespace_and_duplicate_discovery() {
    let a = table("a.b", "same");
    let b = table("a", "b.same");
    let mut g = Graph::default();
    let ab = edge(&a, &b, "ab");
    let ba = edge(&b, &a, "ba");
    let aa = edge(&a, &a, "self");
    g.merge(&a, vec![], vec![ab.clone(), ba.clone(), aa.clone()]);
    g.merge(&b, vec![], vec![ab, ba]);
    assert_eq!(g.nodes.len(), 2);
    assert_eq!(g.edges.len(), 3);
    assert_eq!(g.edges[0].columns.len(), 2);
    assert_ne!(Identity::from(&a).label(), Identity::from(&b).label());
    assert!(g.nodes.iter().all(|n| n.inspected));
}
#[test]
fn diagram_placeholders_do_not_invent_primary_keys_and_pk_fk_precede_other_columns() {
    let a = table("public", "a");
    let b = table("other", "b");
    let mut g = Graph::default();
    let col = |n: &str, pk| Column {
        name: n.into(),
        data_type: "int".into(),
        primary_key: pk,
        nullable: false,
        default_value: None,
    };
    g.merge(
        &a,
        vec![
            col("noise", false),
            col("id", true),
            col("parent_id", false),
        ],
        vec![edge(&a, &b, "fk")],
    );
    assert!(!g.nodes[1].inspected);
    assert!(g.key_columns(1).iter().all(|c| !c.2));
    assert_eq!(g.key_columns(0)[0].0, "id");
    assert!(g.key_columns(0)[1].3);
}
#[test]
fn diagram_bounds_do_not_leave_edges_with_missing_endpoints() {
    let a = table("public", "a");
    let mut g = Graph::default();
    let edges = (0..100)
        .map(|i| edge(&a, &table("public", &format!("b{i}")), &format!("fk{i}")))
        .collect();
    g.merge(&a, vec![], edges);
    assert_eq!(g.nodes.len(), MAX_NODES);
    assert!(g.limited);
    for e in &g.edges {
        assert!(
            g.nodes
                .iter()
                .any(|n| n.identity == Identity::from(&e.referenced))
        );
    }
    let mut g = Graph::default();
    let edges = (0..100)
        .map(|i| edge(&a, &a, &format!("self{i}")))
        .collect();
    g.merge(&a, vec![], edges);
    assert_eq!(g.edges.len(), MAX_EDGES);
    assert!(g.limited);
}
#[test]
fn diagram_rejects_unrelated_and_empty_keys() {
    let a = table("p", "a");
    let b = table("p", "b");
    let mut g = Graph::default();
    let mut empty = edge(&a, &b, "empty");
    empty.columns.clear();
    g.merge(&a, vec![], vec![empty, edge(&b, &b, "unrelated")]);
    assert!(g.edges.is_empty());
    assert_eq!(g.nodes.len(), 1);
}
#[test]
fn diagram_camera_is_bounded_and_centers_selected_table() {
    let mut c = Camera::default();
    c.zoom_by(100.);
    assert_eq!(c.zoom, 1.5);
    c.zoom_by(0.001);
    assert_eq!(c.zoom, 0.5);
    c.pan(100000., -100000.);
    assert_eq!((c.x, c.y), (10000., -10000.));
    let mut g = Graph::default();
    g.ensure_node(&table("p", "a"));
    c.center_node(&g.nodes[0]);
    assert_eq!(g.nodes[0].x * c.zoom + c.x, 24.);
}
