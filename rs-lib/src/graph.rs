// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use std::collections::BTreeMap;
use std::collections::HashMap;

use crate::loader::get_all_specifier_mappers;
use crate::loader::Loader;
use crate::loader::SourceLoader;
use crate::parser::ScopeAnalysisParser;
use crate::specifiers::get_specifiers;
use crate::specifiers::Specifiers;
use crate::MappedSpecifier;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use deno_ast::ModuleSpecifier;
use deno_ast::ParsedSource;
use deno_graph::CapturingModuleAnalyzer;
use deno_graph::Module;
use deno_graph::ModuleGraphError;
use deno_graph::ParsedSourceStore;

pub struct ModuleGraphOptions<'a> {
  pub entry_points: Vec<ModuleSpecifier>,
  pub test_entry_points: Vec<ModuleSpecifier>,
  pub loader: Option<Box<dyn Loader>>,
  pub specifier_mappings: &'a HashMap<ModuleSpecifier, MappedSpecifier>,
  pub import_map: Option<ModuleSpecifier>,
}

/// Wrapper around deno_graph::ModuleGraph.
pub struct ModuleGraph {
  graph: deno_graph::ModuleGraph,
  capturing_analyzer: CapturingModuleAnalyzer,
}

impl ModuleGraph {
  pub async fn build_with_specifiers(
    options: ModuleGraphOptions<'_>,
  ) -> Result<(Self, Specifiers)> {
    let loader = options.loader.unwrap_or_else(|| {
      #[cfg(feature = "tokio-loader")]
      return Box::new(crate::loader::DefaultLoader::new());
      #[cfg(not(feature = "tokio-loader"))]
      panic!("You must provide a loader or use the 'tokio-loader' feature.")
    });
    let resolver = match options.import_map {
      Some(import_map_url) => Some(
        ImportMapResolver::load(&import_map_url, &*loader)
          .await
          .context("Error loading import map.")?,
      ),
      None => None,
    };
    let mut loader = SourceLoader::new(
      loader,
      get_all_specifier_mappers(),
      options.specifier_mappings,
    );
    let source_parser = ScopeAnalysisParser::new();
    let capturing_analyzer =
      CapturingModuleAnalyzer::new(Some(Box::new(source_parser)), None);
    let mut graph = deno_graph::ModuleGraph::new(deno_graph::GraphKind::All);
    graph
      .build(
        options
          .entry_points
          .iter()
          .chain(options.test_entry_points.iter())
          .map(|s| s.to_owned())
          .collect(),
        &mut loader,
        deno_graph::BuildOptions {
          is_dynamic: false,
          imports: vec![],
          resolver: resolver.as_ref().map(|r| r.as_resolver()),
          module_analyzer: Some(&capturing_analyzer),
          reporter: None,
          npm_resolver: None,
        },
      )
      .await;

    let mut error_message = String::new();
    for error in graph.errors() {
      if !error_message.is_empty() {
        error_message.push_str("\n\n");
      }
      error_message.push_str(&error.to_string_with_range());
      if let ModuleGraphError::ModuleError(error) = error {
        if !error_message.contains(error.specifier().as_str()) {
          error_message.push_str(&format!(" ({})", error.specifier()));
        }
      }
    }
    if !error_message.is_empty() {
      bail!("{}", error_message);
    }

    let graph = Self {
      graph,
      capturing_analyzer,
    };

    let loader_specifiers = loader.into_specifiers();

    let not_found_module_mappings = options
      .specifier_mappings
      .iter()
      .filter_map(|(k, v)| match v {
        MappedSpecifier::Package(_) => None,
        MappedSpecifier::Module(_) => Some(k),
      })
      .filter(|s| !loader_specifiers.mapped_modules.contains_key(s))
      .collect::<Vec<_>>();
    if !not_found_module_mappings.is_empty() {
      bail!(
        "The following specifiers were indicated to be mapped to a module, but were not found:\n{}",
        format_specifiers_for_message(not_found_module_mappings),
      );
    }

    let specifiers = get_specifiers(
      &options.entry_points,
      loader_specifiers,
      &graph,
      graph.all_modules(),
    )?;

    let not_found_package_specifiers = options
      .specifier_mappings
      .iter()
      .filter_map(|(k, v)| match v {
        MappedSpecifier::Package(_) => Some(k),
        MappedSpecifier::Module(_) => None,
      })
      .filter(|s| !specifiers.has_mapped(s))
      .collect::<Vec<_>>();
    if !not_found_package_specifiers.is_empty() {
      bail!(
        "The following specifiers were indicated to be mapped to a package, but were not found:\n{}",
        format_specifiers_for_message(not_found_package_specifiers),
      );
    }

    Ok((graph, specifiers))
  }

  pub fn redirects(&self) -> &BTreeMap<ModuleSpecifier, ModuleSpecifier> {
    &self.graph.redirects
  }

  pub fn resolve(&self, specifier: &ModuleSpecifier) -> ModuleSpecifier {
    self.graph.resolve(specifier)
  }

  pub fn get(&self, specifier: &ModuleSpecifier) -> &Module {
    self.graph.get(specifier).unwrap_or_else(|| {
      panic!("dnt bug - Did not find specifier: {}", specifier);
    })
  }

  pub fn get_parsed_source(&self, specifier: &ModuleSpecifier) -> ParsedSource {
    let specifier = self.graph.resolve(specifier);
    self
      .capturing_analyzer
      .get_parsed_source(&specifier)
      .unwrap_or_else(|| {
        panic!(
          "dnt bug - Did not find parsed source for specifier: {}",
          specifier
        );
      })
  }

  pub fn resolve_dependency(
    &self,
    value: &str,
    referrer: &ModuleSpecifier,
  ) -> Option<ModuleSpecifier> {
    self
      .graph
      .resolve_dependency(value, referrer, /* prefer_types */ false)
      .or_else(|| {
        let value_lower = value.to_lowercase();
        if value_lower.starts_with("https://")
          || value_lower.starts_with("http://")
          || value_lower.starts_with("file://")
        {
          ModuleSpecifier::parse(value).ok()
        } else if value_lower.starts_with("./")
          || value_lower.starts_with("../")
        {
          referrer.join(value).ok()
        } else {
          None
        }
      })
  }

  pub fn all_modules(&self) -> impl Iterator<Item = &Module> {
    self.graph.modules()
  }
}

fn format_specifiers_for_message(
  mut specifiers: Vec<&ModuleSpecifier>,
) -> String {
  specifiers.sort();
  specifiers
    .into_iter()
    .map(|s| format!("  * {}", s))
    .collect::<Vec<_>>()
    .join("\n")
}

#[derive(Debug)]
struct ImportMapResolver(import_map::ImportMap);

impl ImportMapResolver {
  pub async fn load(
    import_map_url: &ModuleSpecifier,
    loader: &dyn Loader,
  ) -> Result<Self> {
    let response = loader
      .load(import_map_url.clone())
      .await?
      .ok_or_else(|| anyhow!("Could not find {}", import_map_url))?;
    let result =
      import_map::parse_from_json(import_map_url, &response.content)?;
    // if !result.diagnostics.is_empty() {
    //   todo: surface diagnostics maybe? It seems like this should not be hard error according to import map spec
    //   bail!("Import map diagnostics:\n{}", result.diagnostics.into_iter().map(|d| format!("  - {}", d)).collect::<Vec<_>>().join("\n"));
    //}
    Ok(ImportMapResolver(result.import_map))
  }

  pub fn as_resolver(&self) -> &dyn deno_graph::source::Resolver {
    self
  }
}

impl deno_graph::source::Resolver for ImportMapResolver {
  fn resolve(
    &self,
    specifier: &str,
    referrer: &ModuleSpecifier,
  ) -> Result<ModuleSpecifier, anyhow::Error> {
    self
      .0
      .resolve(specifier, referrer)
      .map_err(|err| err.into())
  }
}
