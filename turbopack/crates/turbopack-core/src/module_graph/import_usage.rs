use anyhow::Result;
use rustc_hash::FxHashSet;
use turbo_tasks::{ResolvedVc, TryJoinIterExt, Vc};

use crate::{
    module::Module,
    module_graph::{GraphEdgeIndex, ModuleGraph, export_usage::compute_export_usage_info},
    reference::ModuleReference,
    resolve::ImportUsage,
};

#[turbo_tasks::value]
#[derive(Clone, Default, Debug)]
pub struct UnusedReferences {
    references: FxHashSet<ResolvedVc<Box<dyn ModuleReference>>>,
    #[turbo_tasks(trace_ignore)]
    edges: FxHashSet<GraphEdgeIndex>,
}

impl UnusedReferences {
    pub fn contains_edge(&self, edge: &GraphEdgeIndex) -> bool {
        self.edges.contains(edge)
    }
    pub fn contains_reference(&self, reference: &ResolvedVc<Box<dyn ModuleReference>>) -> bool {
        self.references.contains(reference)
    }
}

#[turbo_tasks::function(operation)]
pub async fn compute_import_usage_info(
    graph: ResolvedVc<ModuleGraph>,
) -> Result<Vc<UnusedReferences>> {
    // TODO merge compute_import_usage_info and compute_export_usage_info
    // currently compute_export_usage_info is run twice (and has to run twice because the removed
    // references might make some exports unused)
    let export_usage_info = compute_export_usage_info(graph)
        .read_strongly_consistent()
        .await?;

    let mut unused_references_name = FxHashSet::default();
    let mut edges = FxHashSet::default();
    let mut references = FxHashSet::default();
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

                    edges.insert(edge);
                    references.insert(ref_data.reference);
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
    println!("edges: {:#?}", edges);
    println!("references: {:#?}", references);

    Ok(UnusedReferences { edges, references }.cell())
}
