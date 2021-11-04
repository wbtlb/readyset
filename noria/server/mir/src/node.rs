use std::cell::RefCell;
use std::fmt::{Debug, Display, Error, Formatter};

use dataflow::{ops, prelude::ReadySetError};
use node_inner::MirNodeInner;
use nom_sql::analysis::ReferredColumns;
use nom_sql::ColumnSpecification;
use petgraph::graph::NodeIndex;

use std::rc::Rc;

use crate::column::Column;
use crate::{FlowNode, MirNodeRef};
use noria_errors::{internal, internal_err, ReadySetResult};

pub mod node_inner;

/// Helper enum to avoid having separate `make_aggregation_node` and `make_extremum_node` functions
pub enum GroupedNodeType {
    Aggregation(ops::grouped::aggregate::Aggregation),
    Extremum(ops::grouped::extremum::Extremum),
}

pub struct MirNode {
    pub name: String,
    pub from_version: usize,
    pub columns: Vec<Column>,
    pub inner: MirNodeInner,
    pub ancestors: Vec<MirNodeRef>,
    pub children: Vec<MirNodeRef>,
    pub flow_node: Option<FlowNode>,
}

impl MirNode {
    pub fn new(
        name: &str,
        v: usize,
        columns: Vec<Column>,
        inner: MirNodeInner,
        ancestors: Vec<MirNodeRef>,
        children: Vec<MirNodeRef>,
    ) -> MirNodeRef {
        let mn = MirNode {
            name: String::from(name),
            from_version: v,
            columns,
            inner,
            ancestors: ancestors.clone(),
            children,
            flow_node: None,
        };

        let rc_mn = Rc::new(RefCell::new(mn));

        // register as child on ancestors
        for ancestor in &ancestors {
            ancestor.borrow_mut().add_child(rc_mn.clone());
        }

        rc_mn
    }

    /// Adapts an existing `Base`-type MIR Node with the specified column additions and removals.
    pub fn adapt_base(
        node: MirNodeRef,
        added_cols: Vec<&ColumnSpecification>,
        removed_cols: Vec<&ColumnSpecification>,
    ) -> MirNodeRef {
        let over_node = node.borrow();
        match over_node.inner {
            MirNodeInner::Base {
                ref column_specs,
                ref keys,
                ..
            } => {
                let new_column_specs: Vec<(ColumnSpecification, Option<usize>)> = column_specs
                    .iter()
                    .cloned()
                    .filter(|&(ref cs, _)| !removed_cols.contains(&cs))
                    .chain(
                        added_cols
                            .iter()
                            .map(|c| ((*c).clone(), None))
                            .collect::<Vec<(ColumnSpecification, Option<usize>)>>(),
                    )
                    .collect();
                let new_columns: Vec<Column> = new_column_specs
                    .iter()
                    .map(|&(ref cs, _)| Column::from(&cs.column))
                    .collect();

                assert_eq!(
                    new_column_specs.len(),
                    over_node.columns.len() + added_cols.len() - removed_cols.len()
                );

                let new_inner = MirNodeInner::Base {
                    column_specs: new_column_specs,
                    keys: keys.clone(),
                    adapted_over: Some(BaseNodeAdaptation {
                        over: node.clone(),
                        columns_added: added_cols.into_iter().cloned().collect(),
                        columns_removed: removed_cols.into_iter().cloned().collect(),
                    }),
                };
                MirNode::new(
                    &over_node.name,
                    over_node.from_version,
                    new_columns,
                    new_inner,
                    vec![],
                    over_node.children.clone(),
                )
            }
            _ => unreachable!(),
        }
    }

    /// Wraps an existing MIR node into a `Reuse` node.
    /// Note that this does *not* wire the reuse node into ancestors or children of the original
    /// node; if required, this is the responsibility of the caller.
    pub fn reuse(node: MirNodeRef, v: usize) -> MirNodeRef {
        let rcn = node.clone();

        let mn = MirNode {
            name: node.borrow().name.clone(),
            from_version: v,
            columns: node.borrow().columns.clone(),
            inner: MirNodeInner::Reuse { node: rcn },
            ancestors: vec![],
            children: vec![],
            flow_node: None, // will be set in `into_flow_parts`
        };

        Rc::new(RefCell::new(mn))
    }

    pub fn can_reuse_as(&self, for_node: &MirNode) -> bool {
        let mut have_all_columns = true;
        for c in &for_node.columns {
            if !self.columns.contains(c) {
                have_all_columns = false;
                break;
            }
        }

        have_all_columns && self.inner.can_reuse_as(&for_node.inner)
    }

    // currently unused
    #[allow(dead_code)]
    pub fn add_ancestor(&mut self, a: MirNodeRef) {
        self.ancestors.push(a)
    }

    pub fn remove_ancestor(&mut self, a: MirNodeRef) {
        match self
            .ancestors
            .iter()
            .position(|x| x.borrow().versioned_name() == a.borrow().versioned_name())
        {
            None => (),
            Some(idx) => {
                self.ancestors.remove(idx);
            }
        }
    }

    pub fn add_child(&mut self, c: MirNodeRef) {
        self.children.push(c)
    }

    pub fn remove_child(&mut self, a: MirNodeRef) {
        match self
            .children
            .iter()
            .position(|x| x.borrow().versioned_name() == a.borrow().versioned_name())
        {
            None => (),
            Some(idx) => {
                self.children.remove(idx);
            }
        }
    }

    /// Add a new column to the set of emitted columns for this node, and return the resulting index
    /// of that column
    pub fn add_column(&mut self, c: Column) -> ReadySetResult<usize> {
        fn column_pos(node: &MirNode) -> Option<usize> {
            match &node.inner {
                MirNodeInner::Aggregation { .. } => {
                    // the aggregation column must always be the last column
                    Some(node.columns.len() - 1)
                }
                MirNodeInner::Project { emit, .. } => {
                    // New projected columns go before all literals and expressions
                    Some(emit.len())
                }
                MirNodeInner::Filter { .. } | MirNodeInner::TopK { .. } => {
                    // Filters and topk follow the column positioning rules of their parents
                    #[allow(clippy::unwrap_used)] // filters and topk both must have a parent
                    column_pos(&node.ancestors().first().unwrap().borrow())
                }
                _ => None,
            }
        }

        let pos = if let Some(pos) = column_pos(self) {
            self.columns.insert(pos, c.clone());
            pos
        } else {
            self.columns.push(c.clone());
            self.columns.len()
        };

        self.inner.insert_column(c)?;

        Ok(pos)
    }

    pub fn ancestors(&self) -> &[MirNodeRef] {
        self.ancestors.as_slice()
    }

    pub fn children(&self) -> &[MirNodeRef] {
        self.children.as_slice()
    }

    pub fn columns(&self) -> &[Column] {
        self.columns.as_slice()
    }

    /// Finds the source of a child column within the node.
    /// This is currently used for locating the source of a projected column.
    pub fn find_source_for_child_column(&self, child: &Column) -> Option<usize> {
        // we give the alias preference here because in a query like
        // SELECT table1.column1 AS my_alias
        // my_alias will be the column name and "table1.column1" will be the alias.
        // This is slightly backwards from what intuition suggests when you first look at the
        // column struct but means its the "alias" that will exist in the parent node,
        // not the column name.
        if child.aliases.is_empty() {
            self.columns.iter().position(|c| c == child)
        } else {
            self.columns.iter().position(|c| child.aliases.contains(c))
        }
    }

    pub fn column_id_for_column(&self, c: &Column) -> ReadySetResult<usize> {
        #[allow(clippy::cmp_owned)]
        match self.inner {
            // if we're a base, translate to absolute column ID (taking into account deleted
            // columns). We use the column specifications here, which track a tuple of (column
            // spec, absolute column ID).
            // Note that `rposition` is required because multiple columns of the same name might
            // exist if a column has been removed and re-added. We always use the latest column,
            // and assume that only one column of the same name ever exists at the same time.
            MirNodeInner::Base {
                ref column_specs, ..
            } => match column_specs
                .iter()
                .rposition(|cs| Column::from(&cs.0.column) == *c)
            {
                None => Err(ReadySetError::NonExistentColumn {
                    column: c.name.clone(),
                    node: self.name.clone(),
                }),
                Some(id) => Ok(column_specs[id]
                    .1
                    .expect("must have an absolute column ID on base")),
            },
            MirNodeInner::Reuse { ref node } => node.borrow().column_id_for_column(c),
            // otherwise, just look up in the column set
            // Compare by name if there is no table
            _ => match {
                if c.table.is_none() {
                    self.columns.iter().position(|cc| cc.name == c.name)
                } else {
                    self.columns.iter().position(|cc| cc == c)
                }
            } {
                Some(id) => Ok(id),
                None => Err(ReadySetError::NonExistentColumn {
                    column: c.name.clone(),
                    node: self.name.clone(),
                }),
            },
        }
    }

    /// Returns a slice to the column specifications, if this MIR-Node is a base node.
    /// Otherwise, returns `None`.
    pub fn column_specifications(&self) -> ReadySetResult<&[(ColumnSpecification, Option<usize>)]> {
        match self.inner {
            MirNodeInner::Base {
                ref column_specs, ..
            } => Ok(column_specs.as_slice()),
            _ => internal!("Non-base MIR nodes don't have column specifications!"),
        }
    }

    pub fn flow_node_addr(&self) -> ReadySetResult<NodeIndex> {
        match self.flow_node {
            Some(FlowNode::New(na)) | Some(FlowNode::Existing(na)) => Ok(na),
            None => Err(internal_err(format!(
                "MIR node \"{}\" does not have an associated FlowNode",
                self.versioned_name()
            ))),
        }
    }

    #[allow(dead_code)]
    pub fn is_reused(&self) -> bool {
        matches!(self.inner, MirNodeInner::Reuse { .. })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn referenced_columns(&self) -> Vec<Column> {
        // all projected columns
        let mut columns = self.columns.clone();

        // + any parent columns referenced internally by the operator
        match self.inner {
            MirNodeInner::Aggregation { ref on, .. } | MirNodeInner::Extremum { ref on, .. } => {
                // need the "over" column
                if !columns.contains(on) {
                    columns.push(on.clone());
                }
            }
            MirNodeInner::Filter { .. } => {
                let parent = self.ancestors.first().unwrap();
                // need all parent columns
                for c in parent.borrow().columns() {
                    if !columns.contains(c) {
                        columns.push(c.clone());
                    }
                }
            }
            MirNodeInner::Project {
                ref emit,
                ref expressions,
                ..
            } => {
                for c in emit {
                    if !columns.contains(c) {
                        columns.push(c.clone());
                    }
                }
                for (_, expr) in expressions {
                    for c in expr.referred_columns() {
                        if !columns.iter().any(|col| col == c) {
                            columns.push(c.clone().into());
                        }
                    }
                }
            }
            _ => (),
        }
        columns
    }

    pub fn versioned_name(&self) -> String {
        format!("{}_v{}", self.name, self.from_version)
    }

    /// Produce a compact, human-readable description of this node; analogous to the method of the
    /// same name on `Ingredient`.
    pub(crate) fn description(&self) -> String {
        format!(
            "{}: {} / {} columns",
            self.versioned_name(),
            self.inner.description(),
            self.columns.len()
        )
    }
}

/// Specifies the adapatation of an existing base node by column addition/removal.
/// `over` is a `MirNode` of type `Base`.
pub struct BaseNodeAdaptation {
    pub over: MirNodeRef,
    pub columns_added: Vec<ColumnSpecification>,
    pub columns_removed: Vec<ColumnSpecification>,
}

impl Display for MirNode {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(f, "{}", self.inner.description())
    }
}

impl Debug for MirNode {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(
            f,
            "{}, {} ancestors ({}), {} children ({})",
            self.description(),
            self.ancestors.len(),
            self.ancestors
                .iter()
                .map(|a| a.borrow().versioned_name())
                .collect::<Vec<_>>()
                .join(", "),
            self.children.len(),
            self.children
                .iter()
                .map(|c| c.borrow().versioned_name())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    mod find_source_for_child_column {
        use nom_sql::{ColumnSpecification, SqlType};

        use crate::node::node_inner::MirNodeInner;
        use crate::node::MirNode;
        use crate::Column;

        // tests the simple case where the child column has no alias, therefore mapping to the parent
        // column with the same name
        #[test]
        fn with_no_alias() {
            let cspec = |n: &str| -> (ColumnSpecification, Option<usize>) {
                (
                    ColumnSpecification::new(nom_sql::Column::from(n), SqlType::Text),
                    None,
                )
            };

            let parent_columns = vec![Column::from("c1"), Column::from("c2"), Column::from("c3")];

            let a = MirNode {
                name: "a".to_string(),
                from_version: 0,
                columns: parent_columns,
                inner: MirNodeInner::Base {
                    column_specs: vec![cspec("c1"), cspec("c2"), cspec("c3")],
                    keys: vec![Column::from("c1")],
                    adapted_over: None,
                },
                ancestors: vec![],
                children: vec![],
                flow_node: None,
            };

            let child_column = Column::from("c3");

            let idx = a.find_source_for_child_column(&child_column).unwrap();
            assert_eq!(2, idx);
        }

        // tests the case where the child column has an alias, therefore mapping to the parent
        // column with the same name as the alias
        #[test]
        fn with_alias() {
            let c1 = Column {
                table: Some("table".to_string()),
                name: "c1".to_string(),
                function: None,
                aliases: vec![],
            };
            let c2 = Column {
                table: Some("table".to_string()),
                name: "c2".to_string(),
                function: None,
                aliases: vec![],
            };
            let c3 = Column {
                table: Some("table".to_string()),
                name: "c3".to_string(),
                function: None,
                aliases: vec![],
            };

            let child_column = Column {
                table: Some("table".to_string()),
                name: "child".to_string(),
                function: None,
                aliases: vec![Column {
                    table: Some("table".to_string()),
                    name: "c3".to_string(),
                    function: None,
                    aliases: vec![],
                }],
            };

            let cspec = |n: &str| -> (ColumnSpecification, Option<usize>) {
                (
                    ColumnSpecification::new(nom_sql::Column::from(n), SqlType::Text),
                    None,
                )
            };

            let parent_columns = vec![c1, c2, c3];

            let a = MirNode {
                name: "a".to_string(),
                from_version: 0,
                columns: parent_columns,
                inner: MirNodeInner::Base {
                    column_specs: vec![cspec("c1"), cspec("c2"), cspec("c3")],
                    keys: vec![Column::from("c1")],
                    adapted_over: None,
                },
                ancestors: vec![],
                children: vec![],
                flow_node: None,
            };

            let idx = a.find_source_for_child_column(&child_column).unwrap();
            assert_eq!(2, idx);
        }

        // tests the case where the child column is named the same thing as a parent column BUT has an alias.
        // Typically, this alias would map to a different parent column however for testing purposes
        // that column is missing here to ensure it will not match with the wrong column.
        #[test]
        fn with_alias_to_parent_column() {
            let c1 = Column {
                table: Some("table".to_string()),
                name: "c1".to_string(),
                function: None,
                aliases: vec![],
            };

            let child_column = Column {
                table: Some("table".to_string()),
                name: "c1".to_string(),
                function: None,
                aliases: vec![Column {
                    table: Some("table".to_string()),
                    name: "other_name".to_string(),
                    function: None,
                    aliases: vec![],
                }],
            };

            let cspec = |n: &str| -> (ColumnSpecification, Option<usize>) {
                (
                    ColumnSpecification::new(nom_sql::Column::from(n), SqlType::Text),
                    None,
                )
            };

            let parent_columns = vec![c1];

            let a = MirNode {
                name: "a".to_string(),
                from_version: 0,
                columns: parent_columns,
                inner: MirNodeInner::Base {
                    column_specs: vec![cspec("c1")],
                    keys: vec![Column::from("c1")],
                    adapted_over: None,
                },
                ancestors: vec![],
                children: vec![],
                flow_node: None,
            };

            assert_eq!(a.find_source_for_child_column(&child_column), None);
        }
    }

    mod add_column {
        use crate::node::node_inner::MirNodeInner;
        use crate::node::MirNode;
        use crate::Column;
        use crate::MirNodeRef;
        use dataflow::ops::grouped::aggregate::Aggregation as AggregationKind;
        use nom_sql::{BinaryOperator, Expression, Literal};

        fn setup_filter(cond: (usize, Expression)) -> MirNodeRef {
            let cols: Vec<nom_sql::Column> = vec!["x".into(), "agg".into()];

            let condition_expression = Expression::BinaryOp {
                lhs: Box::new(Expression::Column(cols[cond.0].clone())),
                op: BinaryOperator::Equal,
                rhs: Box::new(cond.1),
            };

            let parent = MirNode::new(
                "parent",
                0,
                vec!["x".into(), "agg".into()],
                MirNodeInner::Aggregation {
                    on: "z".into(),
                    group_by: vec!["x".into()],
                    kind: AggregationKind::Count { count_nulls: false },
                },
                vec![],
                vec![],
            );

            // σ [x = 1]
            MirNode::new(
                "filter",
                0,
                vec!["x".into(), "agg".into()],
                MirNodeInner::Filter {
                    conditions: condition_expression,
                },
                vec![parent],
                vec![],
            )
        }

        #[test]
        fn filter_reorders_condition_lhs() {
            let node = setup_filter((1, Expression::Literal(Literal::Integer(1))));

            let condition_expression = Expression::BinaryOp {
                lhs: Box::new(Expression::Column("agg".into())),
                op: BinaryOperator::Equal,
                rhs: Box::new(Expression::Literal(Literal::Integer(1))),
            };

            node.borrow_mut().add_column("y".into()).unwrap();

            assert_eq!(
                node.borrow().columns(),
                vec![Column::from("x"), Column::from("y"), Column::from("agg")]
            );
            match &node.borrow().inner {
                MirNodeInner::Filter { conditions, .. } => {
                    assert_eq!(&condition_expression, conditions);
                }
                _ => unreachable!(),
            };
        }

        #[test]
        fn filter_reorders_condition_comparison_rhs() {
            let node = setup_filter((0, Expression::Column("y".into())));

            let condition_expression = Expression::BinaryOp {
                lhs: Box::new(Expression::Column("x".into())),
                op: BinaryOperator::Equal,
                rhs: Box::new(Expression::Column("y".into())),
            };

            node.borrow_mut().add_column("y".into()).unwrap();

            assert_eq!(
                node.borrow().columns(),
                vec![Column::from("x"), Column::from("y"), Column::from("agg")]
            );
            match &node.borrow().inner {
                MirNodeInner::Filter { conditions, .. } => {
                    assert_eq!(&condition_expression, conditions);
                }
                _ => unreachable!(),
            };
        }

        #[test]
        fn topk_follows_parent_ordering() {
            // count(z) group by (x)
            let parent = MirNode::new(
                "parent",
                0,
                vec!["x".into(), "agg".into()],
                MirNodeInner::Aggregation {
                    on: "z".into(),
                    group_by: vec!["x".into()],
                    kind: AggregationKind::Count { count_nulls: false },
                },
                vec![],
                vec![],
            );

            // TopK γ[x]
            let node = MirNode::new(
                "topk",
                0,
                vec!["x".into(), "agg".into()],
                MirNodeInner::TopK {
                    order: None,
                    group_by: vec!["x".into()],
                    k: 3,
                    offset: 0,
                },
                vec![parent],
                vec![],
            );

            node.borrow_mut().add_column("y".into()).unwrap();

            assert_eq!(
                node.borrow().columns(),
                vec![Column::from("x"), Column::from("y"), Column::from("agg")]
            );
        }
    }
}
