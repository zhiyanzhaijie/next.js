use std::collections::hash_map::Entry;

use anyhow::Result;
use auto_hash_map::AutoSet;
use rustc_hash::{FxHashMap, FxHashSet};
use turbo_rcstr::RcStr;
use turbo_tasks::{ResolvedVc, Vc};

use crate::{
    module::Module,
    module_graph::{GraphEdgeIndex, GraphTraversalAction, ModuleGraph},
    reference::ModuleReference,
    resolve::{ExportUsage, ImportUsage},
};

#[turbo_tasks::value]
#[derive(Clone, Default, Debug)]
pub struct BindingUsageInfo {
    unused_references: FxHashSet<ResolvedVc<Box<dyn ModuleReference>>>,
    #[turbo_tasks(trace_ignore)]
    unused_references_edges: FxHashSet<GraphEdgeIndex>,

    used_exports: FxHashMap<ResolvedVc<Box<dyn Module>>, ModuleExportUsageInfo>,
    export_circuit_breakers: FxHashSet<ResolvedVc<Box<dyn Module>>>,
}

#[turbo_tasks::value(transparent)]
pub struct OptionBindingUsageInfo(Option<ResolvedVc<BindingUsageInfo>>);

#[turbo_tasks::value]
pub struct ModuleExportUsage {
    pub export_usage: ResolvedVc<ModuleExportUsageInfo>,
    // Whether this module exists in an import cycle and has been selected to break the cycle.
    pub is_circuit_breaker: bool,
}
#[turbo_tasks::value_impl]
impl ModuleExportUsage {
    #[turbo_tasks::function]
    pub async fn all() -> Result<Vc<Self>> {
        Ok(Self {
            export_usage: ModuleExportUsageInfo::all().to_resolved().await?,
            is_circuit_breaker: true,
        }
        .cell())
    }
}

impl BindingUsageInfo {
    pub fn is_reference_unused_edge(&self, edge: &GraphEdgeIndex) -> bool {
        self.unused_references_edges.contains(edge)
    }

    pub fn is_reference_unused(&self, reference: &ResolvedVc<Box<dyn ModuleReference>>) -> bool {
        self.unused_references.contains(reference)
    }

    pub async fn used_exports(
        &self,
        module: ResolvedVc<Box<dyn Module>>,
    ) -> Result<Vc<ModuleExportUsage>> {
        let is_circuit_breaker = self.export_circuit_breakers.contains(&module);
        let Some(exports) = self.used_exports.get(&module) else {
            anyhow::bail!(
                "export usage not found for module: {:?}",
                module.ident_string().await?
            );
        };
        Ok(ModuleExportUsage {
            export_usage: exports.clone().resolved_cell(),
            is_circuit_breaker,
        }
        .cell())
    }
}

#[turbo_tasks::function(operation)]
pub async fn compute_binding_usage_info(
    graph: ResolvedVc<ModuleGraph>,
) -> Result<Vc<BindingUsageInfo>> {
    let mut used_exports = FxHashMap::<_, ModuleExportUsageInfo>::default();
    let mut debug_unused_references_name = FxHashSet::<(
        ResolvedVc<Box<dyn Module>>,
        ExportUsage,
        ResolvedVc<Box<dyn Module>>,
    )>::default();
    let mut unused_references_edges = FxHashSet::default();
    let mut unused_references = FxHashSet::default();
    let graph = graph.read_graphs().await?;

    let entries = graph.graphs.iter().flat_map(|g| g.entry_modules());

    graph.traverse_edges_fixed_point_with_priority(
        entries.map(|m| (m, 0)),
        &mut (),
        |parent, target, _| {
            // Entries are always used
            let Some((parent, ref_data, edge)) = parent else {
                used_exports.insert(target, ModuleExportUsageInfo::All);
                return Ok(GraphTraversalAction::Continue);
            };

            // If the current edge is an unused import, skip it
            match &ref_data.import {
                ImportUsage::Exports(exports) => {
                    debug_assert!(!exports.is_empty());
                    let source_used_exports = used_exports.get(&parent).unwrap();
                    if exports
                        .iter()
                        .all(|e| !source_used_exports.is_export_used(e))
                    {
                        debug_unused_references_name.insert((
                            parent,
                            ref_data.export.clone(),
                            target,
                        ));
                        unused_references_edges.insert(edge);
                        unused_references.insert(ref_data.reference);

                        return Ok(GraphTraversalAction::Skip);
                    } else {
                        debug_unused_references_name.remove(&(
                            parent,
                            ref_data.export.clone(),
                            target,
                        ));
                        unused_references_edges.remove(&edge);
                        unused_references.remove(&ref_data.reference);
                        // Continue, add eport
                    }
                }
                ImportUsage::Global => {
                    // Continue, has to always be included
                }
            }

            let entry = used_exports.entry(target);
            let is_first_visit = matches!(entry, Entry::Vacant(_));
            if entry.or_default().add(&ref_data.export) || is_first_visit {
                // First visit, or the used exports changed. This can cause more imports to get used
                // downstream.
                Ok(GraphTraversalAction::Continue)
            } else {
                Ok(GraphTraversalAction::Skip)
            }
        },
        |_, _| Ok(0),
    )?;

    // Compute cycles and select modules to be 'circuit breakers'
    // A circuit breaker module will need to eagerly export lazy getters for its exports to break an
    // evaluation cycle all other modules can export values after defining them
    let mut export_circuit_breakers = FxHashSet::default();
    graph.traverse_cycles(
        |e| e.chunking_type.is_parallel(),
        |cycle| {
            // To break cycles we need to ensure that no importing module can observe a
            // partially populated exports object.

            // We could compute this based on the module graph via a DFS from each entry point
            // to the cycle.  Whatever node is hit first is an entry point to the cycle.
            // (scope hoisting does something similar) and then we would only need to
            // mark 'entry' modules (basically the targets of back edges in the export graph) as
            // circuit breakers.  For now we just mark everything on the theory that cycles are
            // rare.  For vercel-site on 8/22/2025 there were 106 cycles covering 800 modules
            // (or 1.2% of all modules).  So with this analysis we could potentially drop 80% of
            // the cycle breaker modules.
            export_circuit_breakers.extend(cycle.iter().map(|n| **n));
            Ok(())
        },
    )?;

    if std::env::var_os("PRINT_UNUSED_REFERENCES").is_some_and(|v| v == "1" || v == "true") {
        use turbo_tasks::TryJoinIterExt;
        println!(
            "unused_references_name: {:#?}",
            debug_unused_references_name
                .iter()
                .map(async |(s, e, t)| Ok((s.ident_string().await?, e, t.ident_string().await?,)))
                .try_join()
                .await?
        );
    }

    Ok(BindingUsageInfo {
        unused_references,
        unused_references_edges,
        used_exports,
        export_circuit_breakers,
    }
    .cell())
}

#[turbo_tasks::value]
#[derive(Default, Clone, Debug)]
pub enum ModuleExportUsageInfo {
    /// Only the side effects are needed, no exports is used.
    #[default]
    Evaluation,
    Exports(AutoSet<RcStr>),
    All,
}

#[turbo_tasks::value_impl]
impl ModuleExportUsageInfo {
    #[turbo_tasks::function]
    pub fn all() -> Vc<Self> {
        ModuleExportUsageInfo::All.cell()
    }
}

impl ModuleExportUsageInfo {
    /// Merge the given usage into self. Returns true if Self changed.
    pub fn add(&mut self, usage: &ExportUsage) -> bool {
        match (&mut *self, usage) {
            (Self::All, _) => false,
            (_, ExportUsage::All) => {
                *self = Self::All;
                true
            }
            (Self::Evaluation, ExportUsage::Named(name)) => {
                // Promote evaluation to something more specific
                *self = Self::Exports(AutoSet::from_iter([name.clone()]));
                true
            }
            (Self::Exports(l), ExportUsage::Named(r)) => {
                // Merge exports
                l.insert(r.clone())
            }
            (_, ExportUsage::Evaluation) => false,
        }
    }

    pub fn is_export_used(&self, export: &RcStr) -> bool {
        match self {
            Self::All => true,
            Self::Evaluation => false,
            Self::Exports(exports) => exports.contains(export),
        }
    }
}
