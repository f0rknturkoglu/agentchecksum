// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reading a probe suite off disk.
//!
//! The only part of `probes` that touches the filesystem, and the only part that can
//! report a *user input* problem rather than a behavioral one. Everything it produces
//! is a value: after this module runs, a run is pure.
//!
//! The fixture directory is read one level deep. A subdirectory is an error rather
//! than a silent skip — a probe file the tool quietly declined to load would make the
//! gate's coverage claim false — while a non-`.toml` file is skipped by extension
//! alone, which is the one omission that cannot change what was asserted.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{ProbesConfig, normalize_rel_path};
use crate::error::{Error, Result};
use crate::fingerprint::canonical;
use crate::manifest::Digest;
use crate::runner::catalog::ToolCatalog;

use super::digest::suite_digest;
use super::eval::compile_schema;
use super::model::{
    self, DEFAULT_REPEAT, MAX_PROBE_FILES, OutputSchema, ProbeFile, ResolvedExpectations,
    ResolvedProbe,
};
use super::resolve;

/// A loaded suite: every probe resolved, and one identity for the whole set.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeSuite {
    /// The directory the probes were read from.
    pub root: PathBuf,
    /// Every probe, in file order and then declaration order.
    pub probes: Vec<ResolvedProbe>,
    /// The identity a baseline records and a comparison checks first.
    pub digest: Digest,
}

impl ProbeSuite {
    pub fn len(&self) -> usize {
        self.probes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.probes.is_empty()
    }

    pub fn by_name(&self, name: &str) -> Option<&ResolvedProbe> {
        self.probes.iter().find(|probe| probe.name == name)
    }
}

/// Load, validate, resolve and digest a probe suite.
///
/// `repeat_override` is the CLI's `--repeat`. It changes how many samples a run takes
/// and never what a probe asserts, so it is deliberately absent from every digest.
pub fn load_suite(
    config_dir: &Path,
    config: &ProbesConfig,
    catalog: &ToolCatalog,
    repeat_override: Option<u32>,
) -> Result<ProbeSuite> {
    let relative = normalize_rel_path(&config.path).map_err(|_| Error::ConfigInvalid {
        reason: format!(
            "`[probes].path` (`{}`) must be relative to the config file, must not contain `..`, \
             and must not contain a backslash",
            config.path
        ),
    })?;
    let root = config_dir.join(&relative);

    check_repeat(config.repeat, "`[probes].repeat`")?;
    check_repeat(repeat_override, "`--repeat`")?;

    let mut probes: Vec<ResolvedProbe> = Vec::new();
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut schemas: BTreeMap<String, OutputSchema> = BTreeMap::new();

    for (declared_path, path) in probe_files(&root, &relative)? {
        let text = std::fs::read_to_string(&path).map_err(|source| Error::ProbeParse {
            path: path.clone(),
            reason: source.to_string(),
        })?;

        if text.trim().is_empty() {
            return Err(Error::ProbeParse {
                path: path.clone(),
                reason: "the file is empty; a probe file declares at least one `[[probe]]`"
                    .to_string(),
            });
        }

        let file: ProbeFile = toml::from_str(&text).map_err(|source| Error::ProbeParse {
            path: path.clone(),
            reason: source.to_string(),
        })?;
        if file.probe.is_empty() {
            return Err(Error::ProbeParse {
                path: path.clone(),
                reason: "the file declares no `[[probe]]`".to_string(),
            });
        }

        for spec in file.probe {
            let valid = spec.validate(&path)?;

            if let Some(first) = seen.get(&valid.name) {
                return Err(Error::ProbeDuplicate {
                    name: valid.name.clone(),
                    first: PathBuf::from(first),
                    second: PathBuf::from(&declared_path),
                });
            }
            if probes.len() >= model::MAX_PROBES {
                return Err(Error::ProbeInvalid {
                    name: valid.name.clone(),
                    path: path.clone(),
                    reason: format!(
                        "the suite already holds the maximum of {} probes",
                        model::MAX_PROBES
                    ),
                });
            }
            seen.insert(valid.name.clone(), declared_path.clone());

            let resolved = |reference: &str| resolve::resolve_tool(catalog, &valid.name, reference);
            let expect_tool = valid.expect_tool.as_deref().map(resolved).transpose()?;
            let forbid_tools = valid
                .forbid_tools
                .iter()
                .map(|reference| resolved(reference))
                .collect::<Result<Vec<_>>>()?;
            let output_schema = match &valid.output_schema {
                None => None,
                Some(schema) => Some(load_schema(
                    config_dir,
                    schema,
                    &valid.name,
                    &path,
                    &mut schemas,
                )?),
            };

            // The declared repeat is what the probe asks for; the effective repeat is
            // what this run will take. Only the first is part of its identity.
            let declared_repeat = valid.repeat.or(config.repeat).unwrap_or(DEFAULT_REPEAT);
            let repeat = repeat_override
                .or(valid.repeat)
                .or(config.repeat)
                .unwrap_or(DEFAULT_REPEAT);

            probes.push(ResolvedProbe::new(
                valid,
                PathBuf::from(declared_path.clone()),
                ResolvedExpectations {
                    expect_tool,
                    forbid_tools,
                    output_schema,
                },
                repeat,
                declared_repeat,
            )?);
        }
    }

    let digest = suite_digest(&probes)?;
    Ok(ProbeSuite {
        root,
        probes,
        digest,
    })
}

/// The probe files to read, as `(project-relative path, path on disk)`, sorted.
fn probe_files(root: &Path, relative_dir: &str) -> Result<Vec<(String, PathBuf)>> {
    let entries = std::fs::read_dir(root).map_err(|source| Error::Read {
        path: root.to_path_buf(),
        source,
    })?;

    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::Read {
            path: root.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let metadata = std::fs::metadata(&path).map_err(|source| Error::Read {
            path: path.clone(),
            source,
        })?;

        if metadata.is_dir() {
            return Err(Error::ProbeParse {
                path,
                reason:
                    "probe fixtures are one flat directory of `*.toml` files; a subdirectory is \
                         not searched"
                        .to_string(),
            });
        }
        if !metadata.is_file() || path.extension().and_then(OsStr::to_str) != Some("toml") {
            continue;
        }

        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        files.push((format!("{relative_dir}/{name}"), path));
    }

    if files.is_empty() {
        return Err(Error::ConfigInvalid {
            reason: format!(
                "the probe directory `{}` holds no `*.toml` probe file, so there is nothing to \
                 measure",
                root.display()
            ),
        });
    }
    if files.len() > MAX_PROBE_FILES {
        return Err(Error::ConfigInvalid {
            reason: format!(
                "the probe directory `{}` holds {} probe files, more than the maximum of \
                 {MAX_PROBE_FILES}",
                root.display(),
                files.len()
            ),
        });
    }

    // Sorted by the project-relative path, which for one flat directory is also the
    // order a user sees.
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

/// Read one output schema, digest it by content, and prove it compiles.
///
/// Compiling here rather than at evaluation time is deliberate: an unsupported schema
/// should stop a run before it spends model calls on samples nobody can score.
fn load_schema(
    config_dir: &Path,
    relative: &str,
    probe: &str,
    probe_file: &Path,
    cache: &mut BTreeMap<String, OutputSchema>,
) -> Result<OutputSchema> {
    if let Some(cached) = cache.get(relative) {
        return Ok(cached.clone());
    }

    let invalid = |reason: String| Error::ProbeInvalid {
        name: probe.to_string(),
        path: probe_file.to_path_buf(),
        reason,
    };

    let path = config_dir.join(relative);
    let metadata = std::fs::metadata(&path).map_err(|source| {
        invalid(format!(
            "`output_schema` (`{relative}`) could not be read: {source}"
        ))
    })?;
    if metadata.len() > model::MAX_OUTPUT_SCHEMA_BYTES {
        return Err(invalid(format!(
            "`output_schema` (`{relative}`) is {} bytes, more than the maximum of {}",
            metadata.len(),
            model::MAX_OUTPUT_SCHEMA_BYTES
        )));
    }

    let text = std::fs::read_to_string(&path).map_err(|source| {
        invalid(format!(
            "`output_schema` (`{relative}`) could not be read: {source}"
        ))
    })?;
    let schema: Value = serde_json::from_str(&text).map_err(|source| {
        invalid(format!(
            "`output_schema` (`{relative}`) is not valid JSON: {source}"
        ))
    })?;

    compile_schema(&path, &schema)?;

    let schema = OutputSchema {
        relative: relative.to_string(),
        // Content, canonically serialized: reformatting the file is not a change of
        // yardstick.
        digest: Digest::sha256(&canonical::to_vec(&schema)?),
        schema,
    };
    cache.insert(relative.to_string(), schema.clone());
    Ok(schema)
}

/// A repeat is usable or it is a configuration error; there is no clamping, because a
/// run that quietly sampled once instead of never is a run with a different meaning.
fn check_repeat(repeat: Option<u32>, source: &str) -> Result<()> {
    match repeat {
        Some(value) if !(model::MIN_REPEAT..=model::MAX_REPEAT).contains(&value) => {
            Err(Error::ConfigInvalid {
                reason: format!("{source}: {}", model::repeat_problem(value)),
            })
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    use crate::lockfile::Lockfile;
    use crate::manifest::{Dependency, DependencyKind, Facet};

    fn tool_dependency(id: &str) -> Dependency {
        let mut facets = BTreeMap::new();
        facets.insert(
            "input_schema".to_string(),
            Facet {
                digest: Digest::sha256(b"schema"),
                shape: None,
                normalized: Some(json!({
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"]
                })),
            },
        );
        Dependency {
            id: id.to_string(),
            kind: DependencyKind::Tool,
            facets,
            source: None,
        }
    }

    fn catalog() -> ToolCatalog {
        ToolCatalog::from_lockfile(
            &Lockfile::from_dependencies(&[
                tool_dependency("tool:github.search_repositories"),
                tool_dependency("tool:files.delete_file"),
            ])
            .unwrap(),
        )
        .unwrap()
    }

    /// A catalog where two servers expose one remote name.
    fn ambiguous_catalog() -> ToolCatalog {
        ToolCatalog::from_lockfile(
            &Lockfile::from_dependencies(&[
                tool_dependency("tool:one.search"),
                tool_dependency("tool:two.search"),
            ])
            .unwrap(),
        )
        .unwrap()
    }

    struct Project {
        _dir: TempDir,
        root: PathBuf,
    }

    impl Project {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            Self { _dir: dir, root }
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn probes(&self) -> ProbesConfig {
            ProbesConfig {
                path: "probes".to_string(),
                repeat: None,
            }
        }

        fn load(&self) -> Result<ProbeSuite> {
            load_suite(&self.root, &self.probes(), &catalog(), None)
        }

        fn load_with(&self, config: &ProbesConfig, repeat: Option<u32>) -> Result<ProbeSuite> {
            load_suite(&self.root, config, &catalog(), repeat)
        }
    }

    fn probe_text(name: &str, expectations: &str) -> String {
        format!("[[probe]]\nname = \"{name}\"\nprompt = \"Ask something.\"\n{expectations}")
    }

    #[test]
    fn a_suite_loads_in_file_order_then_declaration_order() {
        let project = Project::new();
        project.write(
            "probes/b.toml",
            &probe_text("b-one", "expect_tool = \"search_repositories\""),
        );
        project.write(
            "probes/a.toml",
            &format!(
                "{}\n{}",
                probe_text("a-one", "expect_tool = \"search_repositories\""),
                probe_text("a-two", "forbid_tools = [\"delete_file\"]")
            ),
        );

        let suite = project.load().unwrap();

        assert_eq!(
            suite
                .probes
                .iter()
                .map(|probe| probe.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a-one", "a-two", "b-one"]
        );
        assert_eq!(suite.len(), 3);
        assert!(!suite.is_empty());
        assert_eq!(
            suite.by_name("a-two").unwrap().path,
            PathBuf::from("probes/a.toml")
        );
        assert_eq!(suite.root, project.root.join("probes"));
        // The resolved probe carries canonical ids, not the references in the file.
        assert_eq!(
            suite
                .by_name("a-one")
                .unwrap()
                .expect_tool
                .as_ref()
                .unwrap()
                .id,
            "tool:github.search_repositories"
        );
        assert_eq!(
            suite.by_name("a-two").unwrap().forbid_tools[0].id,
            "tool:files.delete_file"
        );
    }

    #[test]
    fn a_probe_file_that_does_not_change_the_assertion_keeps_the_identity() {
        let first = Project::new();
        first.write(
            "probes/a.toml",
            &probe_text("search", "expect_tool = \"search_repositories\""),
        );
        let second = Project::new();
        second.write(
            "probes/renamed.toml",
            &probe_text("search", "expect_tool = \"search_repositories\""),
        );

        assert_eq!(
            first.load().unwrap().digest,
            second.load().unwrap().digest,
            "file layout is not part of the suite's identity"
        );
        // Two loads of one project agree.
        assert_eq!(first.load().unwrap().digest, first.load().unwrap().digest);
    }

    #[test]
    fn non_toml_files_are_ignored_by_extension_alone() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("search", "expect_tool = \"search_repositories\""),
        );
        project.write("probes/README.md", "# probes\n");
        project.write("probes/.DS_Store", "not really a store");
        project.write("probes/notes.txt", "irrelevant");

        assert_eq!(project.load().unwrap().len(), 1);
    }

    #[test]
    fn a_subdirectory_is_an_error_rather_than_a_silent_skip() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("search", "expect_tool = \"search_repositories\""),
        );
        project.write(
            "probes/nested/b.toml",
            &probe_text("hidden", "expect_tool = \"search_repositories\""),
        );

        let error = project.load().unwrap_err();
        assert!(matches!(error, Error::ProbeParse { .. }), "{error:?}");
        assert!(error.to_string().contains("nested"), "{error}");
    }

    #[test]
    fn a_missing_probe_directory_is_an_error() {
        let project = Project::new();

        let error = project.load().unwrap_err();
        assert!(matches!(error, Error::Read { .. }), "{error:?}");
    }

    #[test]
    fn a_probe_directory_with_no_probe_files_is_an_error() {
        let project = Project::new();
        std::fs::create_dir_all(project.root.join("probes")).unwrap();

        let error = project.load().unwrap_err();
        assert!(matches!(error, Error::ConfigInvalid { .. }), "{error:?}");
        assert!(error.to_string().contains("no `*.toml`"), "{error}");
    }

    #[test]
    fn an_empty_probe_file_is_an_error() {
        let project = Project::new();
        project.write("probes/a.toml", "");
        assert!(matches!(
            project.load().unwrap_err(),
            Error::ProbeParse { .. }
        ));

        project.write("probes/a.toml", "\n   \t\n");
        let error = project.load().unwrap_err();
        assert!(error.to_string().contains("empty"), "{error}");
    }

    #[test]
    fn a_probe_file_without_probes_is_an_error() {
        let project = Project::new();
        project.write("probes/a.toml", "# nothing to see here\n");

        let error = project.load().unwrap_err();
        assert!(matches!(error, Error::ProbeParse { .. }), "{error:?}");
        assert!(error.to_string().contains("no `[[probe]]`"), "{error}");
    }

    #[test]
    fn invalid_toml_is_a_parse_error() {
        let project = Project::new();
        project.write("probes/a.toml", "[[probe]]\nname = \n");

        assert!(matches!(
            project.load().unwrap_err(),
            Error::ProbeParse { .. }
        ));
    }

    #[test]
    fn an_unknown_key_is_a_parse_error_that_names_it() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            "[[probe]]\nname = \"a\"\nprompt = \"p\"\nexpect_toll = \"typo\"\n",
        );

        let error = project.load().unwrap_err();
        assert!(matches!(error, Error::ProbeParse { .. }), "{error:?}");
        assert!(error.to_string().contains("expect_toll"), "{error}");
    }

    #[test]
    fn a_duplicate_name_is_refused_and_names_both_files() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("search", "expect_tool = \"search_repositories\""),
        );
        project.write(
            "probes/b.toml",
            &probe_text("search", "forbid_tools = [\"delete_file\"]"),
        );

        let error = project.load().unwrap_err();
        match error {
            Error::ProbeDuplicate {
                name,
                first,
                second,
            } => {
                assert_eq!(name, "search");
                assert_eq!(first, PathBuf::from("probes/a.toml"));
                assert_eq!(second, PathBuf::from("probes/b.toml"));
            }
            other => panic!("expected ProbeDuplicate, got {other:?}"),
        }
    }

    #[test]
    fn too_many_probe_files_are_refused() {
        let project = Project::new();
        for index in 0..=MAX_PROBE_FILES {
            project.write(
                &format!("probes/{index:03}.toml"),
                &probe_text(
                    &format!("p{index}"),
                    "expect_tool = \"search_repositories\"",
                ),
            );
        }

        let error = project.load().unwrap_err();
        match error {
            Error::ConfigInvalid { reason } => assert!(reason.contains("more than"), "{reason}"),
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
    }

    #[test]
    fn too_many_probes_are_refused() {
        let project = Project::new();
        let mut text = String::new();
        for index in 0..=model::MAX_PROBES {
            text.push_str(&probe_text(
                &format!("p{index}"),
                "expect_tool = \"search_repositories\"",
            ));
            text.push('\n');
        }
        project.write("probes/a.toml", &text);

        let error = project.load().unwrap_err();
        match error {
            Error::ProbeInvalid { reason, .. } => {
                assert!(reason.contains("maximum of 256"), "{reason}")
            }
            other => panic!("expected ProbeInvalid, got {other:?}"),
        }
    }

    #[test]
    fn repeat_precedence_is_cli_then_probe_then_config_then_one() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &format!(
                "{}\n{}",
                probe_text(
                    "declared",
                    "repeat = 2\nexpect_tool = \"search_repositories\""
                ),
                probe_text("inherited", "expect_tool = \"search_repositories\"")
            ),
        );

        let configured = ProbesConfig {
            path: "probes".to_string(),
            repeat: Some(4),
        };
        let suite = project.load_with(&configured, Some(7)).unwrap();
        assert_eq!(suite.by_name("declared").unwrap().repeat, 7);
        assert_eq!(suite.by_name("declared").unwrap().declared_repeat, 2);
        assert_eq!(suite.by_name("inherited").unwrap().repeat, 7);
        assert_eq!(suite.by_name("inherited").unwrap().declared_repeat, 4);

        let suite = project.load_with(&configured, None).unwrap();
        assert_eq!(suite.by_name("declared").unwrap().repeat, 2);
        assert_eq!(suite.by_name("inherited").unwrap().repeat, 4);

        let suite = project.load().unwrap();
        assert_eq!(suite.by_name("declared").unwrap().repeat, 2);
        assert_eq!(
            suite.by_name("inherited").unwrap().repeat,
            DEFAULT_REPEAT,
            "nothing configured means one sample"
        );
    }

    #[test]
    fn the_cli_repeat_override_moves_the_sample_count_but_not_the_identity() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("search", "expect_tool = \"search_repositories\""),
        );

        let once = project.load().unwrap();
        let many = project.load_with(&project.probes(), Some(9)).unwrap();

        assert_eq!(once.probes[0].repeat, 1);
        assert_eq!(many.probes[0].repeat, 9);
        assert_eq!(once.digest, many.digest);
        assert_eq!(once.probes[0].digest, many.probes[0].digest);
    }

    #[test]
    fn the_configured_repeat_does_move_the_identity() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("search", "expect_tool = \"search_repositories\""),
        );

        let one = project.load().unwrap();
        let four = project
            .load_with(
                &ProbesConfig {
                    path: "probes".to_string(),
                    repeat: Some(4),
                },
                None,
            )
            .unwrap();

        assert_ne!(one.probes[0].digest, four.probes[0].digest);
        assert_ne!(one.digest, four.digest);
    }

    #[test]
    fn a_repeat_outside_the_bounds_is_a_configuration_error() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("search", "expect_tool = \"search_repositories\""),
        );

        for repeat in [0, 101] {
            let error = project
                .load_with(
                    &ProbesConfig {
                        path: "probes".to_string(),
                        repeat: Some(repeat),
                    },
                    None,
                )
                .unwrap_err();
            assert!(matches!(error, Error::ConfigInvalid { .. }), "{error:?}");
            assert!(error.to_string().contains("[probes].repeat"), "{error}");

            let error = project
                .load_with(&project.probes(), Some(repeat))
                .unwrap_err();
            assert!(error.to_string().contains("--repeat"), "{error}");
        }
    }

    #[test]
    fn an_invalid_probe_path_is_a_configuration_error() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("search", "expect_tool = \"search_repositories\""),
        );

        for path in ["/etc/probes", "../probes", "probes\\nested", ""] {
            let error = project
                .load_with(
                    &ProbesConfig {
                        path: path.to_string(),
                        repeat: None,
                    },
                    None,
                )
                .unwrap_err();
            assert!(
                matches!(error, Error::ConfigInvalid { .. }),
                "{path}: {error:?}"
            );
        }
    }

    #[test]
    fn an_output_schema_is_loaded_and_contributes_its_content() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("structured", "output_schema = \"schemas/result.json\""),
        );
        project.write(
            "schemas/result.json",
            r##"{ "$defs": { "answer": { "type": "string" } },
                 "type": "object",
                 "properties": { "answer": { "$ref": "#/$defs/answer" } } }"##,
        );

        let suite = project.load().unwrap();
        let schema = suite.probes[0].output_schema.as_ref().unwrap();

        assert_eq!(schema.relative, "schemas/result.json");
        assert_eq!(schema.schema["type"], "object");
        // Reformatting the same schema is not a change of yardstick: the digest is
        // over the canonical JSON, so member order and whitespace cannot move it.
        let reformatted = serde_json::from_str::<Value>(
            r##"{"type":"object","properties":{"answer":{"$ref":"#/$defs/answer"}},
                 "$defs":{"answer":{"type":"string"}}}"##,
        )
        .unwrap();
        assert_eq!(
            canonical::to_vec(&schema.schema).unwrap(),
            canonical::to_vec(&reformatted).unwrap()
        );
        assert_eq!(
            schema.digest,
            Digest::sha256(&canonical::to_vec(&reformatted).unwrap())
        );
    }

    #[test]
    fn a_missing_output_schema_names_the_probe_and_the_schema() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("structured", "output_schema = \"schemas/absent.json\""),
        );

        let error = project.load().unwrap_err();
        match error {
            Error::ProbeInvalid { name, path, reason } => {
                assert_eq!(name, "structured");
                assert_eq!(path, project.root.join("probes/a.toml"));
                assert!(reason.contains("schemas/absent.json"), "{reason}");
            }
            other => panic!("expected ProbeInvalid, got {other:?}"),
        }
    }

    #[test]
    fn an_output_schema_that_is_not_json_is_refused() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("structured", "output_schema = \"schemas/result.json\""),
        );
        project.write("schemas/result.json", "{ not a schema");

        let error = project.load().unwrap_err();
        assert!(matches!(error, Error::ProbeInvalid { .. }), "{error:?}");
        assert!(error.to_string().contains("not valid JSON"), "{error}");
    }

    #[test]
    fn an_oversized_output_schema_is_refused_rather_than_truncated() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("structured", "output_schema = \"schemas/result.json\""),
        );
        let padded = "x".repeat(model::MAX_OUTPUT_SCHEMA_BYTES as usize + 1);
        project.write(
            "schemas/result.json",
            &format!("{{ \"type\": \"object\", \"description\": \"{padded}\" }}"),
        );

        let error = project.load().unwrap_err();
        assert!(matches!(error, Error::ProbeInvalid { .. }), "{error:?}");
        assert!(
            error.to_string().contains("more than the maximum"),
            "{error}"
        );
    }

    #[test]
    fn an_output_schema_that_needs_an_external_reference_is_unsupported_not_a_fetch() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("structured", "output_schema = \"schemas/result.json\""),
        );
        project.write(
            "schemas/result.json",
            r#"{ "$ref": "https://example.com/remote.json" }"#,
        );

        let error = project.load().unwrap_err();
        match error {
            Error::SchemaUnsupported { path, .. } => {
                assert_eq!(path, project.root.join("schemas/result.json"));
            }
            other => panic!("expected SchemaUnsupported, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_tool_reference_is_refused_wherever_it_appears() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("expected", "expect_tool = \"search_repoz\""),
        );
        project.write(
            "probes/b.toml",
            &probe_text("forbidden", "forbid_tools = [\"delete_everything\"]"),
        );

        // The first file is read first, so the expected-tool reference fails there.
        let error = project.load().unwrap_err();
        match error {
            Error::ProbeToolUnknown { probe, reference } => {
                assert_eq!(probe, "expected");
                assert_eq!(reference, "search_repoz");
            }
            other => panic!("expected ProbeToolUnknown, got {other:?}"),
        }

        // `forbid_tools` goes through the same resolver.
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("forbidden", "forbid_tools = [\"search\"]"),
        );
        let error =
            load_suite(&project.root, &project.probes(), &ambiguous_catalog(), None).unwrap_err();
        match error {
            Error::ProbeToolAmbiguous {
                probe,
                reference,
                matches,
                candidates,
            } => {
                assert_eq!(probe, "forbidden");
                assert_eq!(reference, "search");
                assert_eq!(matches, 2);
                assert!(candidates.contains("tool:one.search"), "{candidates}");
            }
            other => panic!("expected ProbeToolAmbiguous, got {other:?}"),
        }
    }

    #[test]
    fn an_ambiguous_reference_is_refused_with_its_candidates() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text("search", "expect_tool = \"search\""),
        );

        let error =
            load_suite(&project.root, &project.probes(), &ambiguous_catalog(), None).unwrap_err();
        assert!(
            matches!(error, Error::ProbeToolAmbiguous { matches: 2, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_probe_invalid_expectation_is_rejected_before_any_resolution() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &probe_text(
                "search",
                "expect_tool = \"search_repositories\"\nexpect_args = { q = { equals = 1, one_of = [1] } }",
            ),
        );

        let error = project.load().unwrap_err();
        match error {
            Error::ProbeInvalid { name, reason, .. } => {
                assert_eq!(name, "search");
                assert!(reason.contains("operators"), "{reason}");
            }
            other => panic!("expected ProbeInvalid, got {other:?}"),
        }
    }

    #[test]
    fn one_schema_file_shared_by_two_probes_means_the_same_to_both() {
        let project = Project::new();
        project.write(
            "probes/a.toml",
            &format!(
                "{}\n{}",
                probe_text("first", "output_schema = \"schemas/result.json\""),
                probe_text("second", "output_schema = \"schemas/result.json\"")
            ),
        );
        project.write("schemas/result.json", r#"{ "type": "object" }"#);

        let suite = project.load().unwrap();
        let first = suite
            .by_name("first")
            .unwrap()
            .output_schema
            .as_ref()
            .unwrap();
        let second = suite
            .by_name("second")
            .unwrap()
            .output_schema
            .as_ref()
            .unwrap();

        assert_eq!(first.digest, second.digest);
        assert_eq!(first.relative, second.relative);
    }
}
