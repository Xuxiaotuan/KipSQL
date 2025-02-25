use std::collections::HashSet;
use itertools::Itertools;
use lazy_static::lazy_static;
use crate::expression::ScalarExpression;
use crate::optimizer::core::opt_expr::OptExprNode;
use crate::optimizer::core::pattern::{Pattern, PatternChildrenPredicate};
use crate::optimizer::core::rule::Rule;
use crate::optimizer::heuristic::graph::{HepGraph, HepNodeId};
use crate::planner::operator::Operator;
use crate::planner::operator::project::ProjectOperator;
use crate::types::ColumnId;

lazy_static! {
    static ref PUSH_PROJECT_INTO_SCAN_RULE: Pattern = {
        Pattern {
            predicate: |op| matches!(op, Operator::Project(_)),
            children: PatternChildrenPredicate::Predicate(vec![Pattern {
                predicate: |op| matches!(op, Operator::Scan(_)),
                children: PatternChildrenPredicate::None,
            }]),
        }
    };

    static ref PUSH_PROJECT_THROUGH_CHILD_RULE: Pattern = {
        Pattern {
            predicate: |op| matches!(op, Operator::Project(_)),
            children: PatternChildrenPredicate::Predicate(vec![Pattern {
                predicate: |op| !matches!(op, Operator::Scan(_) | Operator::Project(_)),
                children: PatternChildrenPredicate::Predicate(vec![Pattern {
                    predicate: |op| !matches!(op, Operator::Project(_)),
                    children: PatternChildrenPredicate::None,
                }]),
            }]),
        }
    };
}

#[derive(Copy, Clone)]
pub struct PushProjectIntoScan;

impl Rule for PushProjectIntoScan {
    fn pattern(&self) -> &Pattern {
        &PUSH_PROJECT_INTO_SCAN_RULE
    }

    fn apply(&self, node_id: HepNodeId, graph: &mut HepGraph) {
        if let Operator::Project(project_op) = graph.operator(node_id) {
            let child_index = graph.children_at(node_id)[0];
            if let Operator::Scan(scan_op) = graph.operator(child_index) {
                let mut new_scan_op = scan_op.clone();

                new_scan_op.columns = project_op.columns
                    .iter()
                    .filter(|expr| matches!(expr.unpack_alias(),ScalarExpression::ColumnRef(_)))
                    .cloned()
                    .collect_vec();

                graph.remove_node(node_id, false);
                graph.replace_node(
                    child_index,
                    OptExprNode::OperatorRef(Operator::Scan(new_scan_op))
                );
            }
        }
    }
}

#[derive(Clone)]
pub struct PushProjectThroughChild;

impl Rule for PushProjectThroughChild {
    fn pattern(&self) -> &Pattern {
        &PUSH_PROJECT_THROUGH_CHILD_RULE
    }

    fn apply(&self, node_id: HepNodeId, graph: &mut HepGraph) {
        let node_operator = graph.operator(node_id);
        let input_refs = node_operator.project_input_refs();

        if let Operator::Project(_) = node_operator {
            let child_index = graph.children_at(node_id)[0];
            let mut node_referenced_columns = node_operator.referenced_columns();
            let child_operator = graph.operator(child_index);
            let child_referenced_columns = child_operator.referenced_columns();
            let is_child_agg = matches!(child_operator, Operator::Aggregate(_));

            // When the aggregate function is a child node,
            // the pushdown will lose the corresponding ColumnRef due to `InputRef`.
            // Therefore, it is necessary to map the InputRef to the corresponding ColumnRef
            // and push it down.
            if is_child_agg && !input_refs.is_empty() {
                node_referenced_columns.append(
                    &mut child_operator.agg_mapping_col_refs(&input_refs)
                )
            }

            let intersection_columns_ids = child_referenced_columns
                .iter()
                .map(|col| col.id)
                .chain(
                    node_referenced_columns
                        .iter()
                        .map(|col| col.id)
                )
                .collect::<HashSet<ColumnId>>();

            if intersection_columns_ids.is_empty() {
                return;
            }

            for grandson_id in graph.children_at(child_index) {
                let mut columns = graph.operator(grandson_id)
                    .referenced_columns()
                    .into_iter()
                    .unique_by(|col| col.id)
                    .filter(|u| intersection_columns_ids.contains(&u.id))
                    .map(|col| ScalarExpression::ColumnRef(col))
                    .collect_vec();

                if !is_child_agg && !input_refs.is_empty() {
                    // Tips: Aggregation InputRefs fields take precedence
                    columns = input_refs.iter()
                        .cloned()
                        .chain(columns)
                        .collect_vec();
                }

                if !columns.is_empty() {
                    graph.add_node(
                        child_index,
                        Some(grandson_id),
                        OptExprNode::OperatorRef(
                            Operator::Project(ProjectOperator { columns })
                        )
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::binder::test::select_sql_run;
    use crate::execution::ExecutorError;
    use crate::optimizer::heuristic::batch::{HepBatchStrategy};
    use crate::optimizer::heuristic::optimizer::HepOptimizer;
    use crate::optimizer::rule::RuleImpl;
    use crate::planner::operator::join::JoinCondition;
    use crate::planner::operator::Operator;

    #[tokio::test]
    async fn test_project_into_table_scan() -> Result<(), ExecutorError> {
        let plan = select_sql_run("select * from t1").await?;

        let best_plan = HepOptimizer::new(plan.clone())
            .batch(
                "test_project_into_table_scan".to_string(),
                HepBatchStrategy::once_topdown(),
                vec![RuleImpl::PushProjectIntoScan]
            )
            .find_best();

        assert_eq!(best_plan.childrens.len(), 0);
        match best_plan.operator {
            Operator::Scan(op) => {
                assert_eq!(op.columns.len(), 2);
            },
            _ => unreachable!("Should be a scan operator"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_project_through_child_on_join() -> Result<(), ExecutorError> {
        let plan = select_sql_run("select c1, c3 from t1 left join t2 on c1 = c3").await?;

        let best_plan = HepOptimizer::new(plan.clone())
            .batch(
                "test_project_through_child_on_join".to_string(),
                HepBatchStrategy::fix_point_topdown(10),
                vec![
                    RuleImpl::PushProjectThroughChild,
                    RuleImpl::PushProjectIntoScan
                ]
            ).find_best();

        assert_eq!(best_plan.childrens.len(), 1);
        match best_plan.operator {
            Operator::Project(op) => {
                assert_eq!(op.columns.len(), 2);
            },
            _ => unreachable!("Should be a project operator"),
        }
        match &best_plan.childrens[0].operator {
            Operator::Join(op) => {
                match &op.on {
                    JoinCondition::On { on, filter } => {
                        assert_eq!(on.len(), 1);
                        assert!(filter.is_none());
                    }
                    _ => unreachable!("Should be a on condition"),
                }
            },
            _ => unreachable!("Should be a join operator"),
        }

        assert_eq!(best_plan.childrens[0].childrens.len(), 2);

        for grandson_plan in &best_plan.childrens[0].childrens {
            match &grandson_plan.operator {
                Operator::Scan(op) => {
                    assert_eq!(op.columns.len(), 1);
                },
                _ => unreachable!("Should be a scan operator"),
            }
        }

        Ok(())
    }
}