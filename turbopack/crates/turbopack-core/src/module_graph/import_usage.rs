use anyhow::Result;
use petgraph::graph::EdgeIndex;
use rustc_hash::FxHashSet;
use turbo_tasks::{ResolvedVc, TryJoinIterExt, Vc};

use crate::{
    module::Module,
    module_graph::{ModuleGraph, export_usage::compute_export_usage_info},
    resolve::ImportUsage,
};

#[turbo_tasks::value]
pub struct UnusedReferences(#[turbo_tasks(trace_ignore)] FxHashSet<EdgeIndex>);

#[turbo_tasks::function(operation)]
pub async fn compute_import_usage_info(
    graph: ResolvedVc<ModuleGraph>,
) -> Result<Vc<UnusedReferences>> {
    let export_usage_info = compute_export_usage_info(graph)
        .read_strongly_consistent()
        .await?;

    let mut unused_references_name = FxHashSet::<_>::default();
    let mut unused_references = FxHashSet::<_>::default();
    let graph = graph.read_graphs().await?;
    graph.traverse_all_edges_unordered(|(source, ref_data, edge), target| {
        match &ref_data.import {
            ImportUsage::Global => {
                // has to always be included
            }
            ImportUsage::Exports(exports) => {
                debug_assert!(!exports.is_empty());
                let source_used_exports = export_usage_info.used_exports_ref(source);

                if exports
                    .iter()
                    .all(|e| !source_used_exports.is_export_used(e))
                {
                    unused_references_name.insert((source, ref_data.export.clone(), target));
                    unused_references.insert(edge);
                }
            }
        }

        Ok(())
    })?;

    println!(
        "unused_references_name: {:#?}",
        unused_references_name
            .iter()
            .map(async |(s, e, t)| Ok((s.ident_string().await?, e, t.ident_string().await?,)))
            .try_join()
            .await?
    );

    Ok(UnusedReferences(unused_references).cell())
}
