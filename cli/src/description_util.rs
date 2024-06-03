use std::collections::HashMap;

use itertools::Itertools;
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merged_tree::MergedTree;
use jj_lib::repo::ReadonlyRepo;
use jj_lib::settings::UserSettings;

use crate::cli_util::{edit_temp_file, short_commit_hash, WorkspaceCommandHelper};
use crate::command_error::CommandError;
use crate::diff_util::DiffFormat;
use crate::formatter::PlainTextFormatter;
use crate::text_util;
use crate::ui::Ui;

/// Cleanup a description by normalizing line endings, and removing leading and
/// trailing blank lines.
fn cleanup_description(description: &str) -> String {
    let description = description
        .lines()
        .filter(|line| !line.starts_with("JJ: "))
        .join("\n");
    text_util::complete_newline(description.trim_matches('\n'))
}

pub fn edit_description(
    repo: &ReadonlyRepo,
    description: &str,
    settings: &UserSettings,
) -> Result<String, CommandError> {
    let description = format!(
        r#"{}
JJ: Lines starting with "JJ: " (like this one) will be removed.
"#,
        description
    );

    let description = edit_temp_file(
        "description",
        ".jjdescription",
        repo.repo_path(),
        &description,
        settings,
    )?;

    Ok(cleanup_description(&description))
}

#[derive(Debug)]
pub struct EditMultipleDescriptionsResult {
    /// The parsed, formatted descriptions.
    pub descriptions: HashMap<CommitId, String>,
    /// Commit IDs that were expected while parsing the edited messages, but
    /// which were not found.
    pub missing: Vec<String>,
    /// Commit IDs that were found multiple times while parsing the edited
    /// messages.
    pub duplicates: Vec<String>,
    /// Commit IDs that were found while parsing the edited messages, but which
    /// were not originally being edited.
    pub unexpected: Vec<String>,
}

/// Edits the descriptions of the given commits in a single editor session.
pub fn edit_multiple_descriptions(
    ui: &Ui,
    settings: &UserSettings,
    workspace_command: &WorkspaceCommandHelper,
    repo: &ReadonlyRepo,
    commits: &[&Commit],
) -> Result<EditMultipleDescriptionsResult, CommandError> {
    let mut commits_map = HashMap::new();
    let mut output_chunks = Vec::new();

    for &commit in commits.iter() {
        let commit_hash = short_commit_hash(commit.id());
        if commits.len() > 1 {
            output_chunks.push(format!("JJ: describe {}\n", commit_hash.clone()));
        }
        commits_map.insert(commit_hash, commit.id());
        let template = description_template_for_describe(ui, settings, workspace_command, commit)?;
        output_chunks.push(template);
        output_chunks.push("\n".to_owned());
    }
    output_chunks
        .push("JJ: Lines starting with \"JJ: \" (like this one) will be removed.\n".to_owned());
    let bulk_message = output_chunks.join("");

    let bulk_message = edit_temp_file(
        "description",
        ".jjdescription",
        repo.repo_path(),
        &bulk_message,
        settings,
    )?;

    if commits.len() == 1 {
        return Ok(EditMultipleDescriptionsResult {
            descriptions: HashMap::from([(
                commits[0].id().clone(),
                cleanup_description(&bulk_message),
            )]),
            missing: vec![],
            duplicates: vec![],
            unexpected: vec![],
        });
    }

    Ok(parse_bulk_edit_message(&bulk_message, &commits_map))
}

/// Parse the bulk message of edited commit descriptions.
fn parse_bulk_edit_message(
    message: &str,
    commit_ids_map: &HashMap<String, &CommitId>,
) -> EditMultipleDescriptionsResult {
    let mut descriptions = HashMap::new();
    let mut duplicates = Vec::new();
    let mut unexpected = Vec::new();

    let messages = message.lines().fold(vec![], |mut accum, line| {
        if let Some(commit_id_prefix) = line.strip_prefix("JJ: describe ") {
            accum.push((commit_id_prefix, vec![]));
        } else if let Some((_, lines)) = accum.last_mut() {
            lines.push(line);
        };
        accum
    });

    for (commit_id_prefix, description_lines) in messages {
        let commit_id = match commit_ids_map.get(commit_id_prefix) {
            Some(&commit_id) => commit_id,
            None => {
                unexpected.push(commit_id_prefix.to_string());
                continue;
            }
        };
        if descriptions.contains_key(commit_id) {
            duplicates.push(commit_id_prefix.to_string());
            continue;
        }
        descriptions.insert(
            commit_id.clone(),
            cleanup_description(&description_lines.join("\n")),
        );
    }

    let missing: Vec<_> = commit_ids_map
        .keys()
        .filter_map(|commit_id_prefix| {
            let commit_id = match commit_ids_map.get(commit_id_prefix) {
                Some(&commit_id) => commit_id,
                None => {
                    return None;
                }
            };
            if !descriptions.contains_key(commit_id) {
                Some(commit_id_prefix.to_string())
            } else {
                None
            }
        })
        .collect();

    EditMultipleDescriptionsResult {
        descriptions,
        missing,
        duplicates,
        unexpected,
    }
}

/// Combines the descriptions from the input commits. If only one is non-empty,
/// then that one is used. Otherwise we concatenate the messages and ask the
/// user to edit the result in their editor.
pub fn combine_messages(
    repo: &ReadonlyRepo,
    sources: &[&Commit],
    destination: &Commit,
    settings: &UserSettings,
) -> Result<String, CommandError> {
    let non_empty = sources
        .iter()
        .chain(std::iter::once(&destination))
        .filter(|c| !c.description().is_empty())
        .take(2)
        .collect_vec();
    match *non_empty.as_slice() {
        [] => {
            return Ok(String::new());
        }
        [commit] => {
            return Ok(commit.description().to_owned());
        }
        _ => {}
    }
    // Produce a combined description with instructions for the user to edit.
    // Include empty descriptins too, so the user doesn't have to wonder why they
    // only see 2 descriptions when they combined 3 commits.
    let mut combined = "JJ: Enter a description for the combined commit.".to_string();
    combined.push_str("\nJJ: Description from the destination commit:\n");
    combined.push_str(destination.description());
    for commit in sources {
        combined.push_str("\nJJ: Description from source commit:\n");
        combined.push_str(commit.description());
    }
    edit_description(repo, &combined, settings)
}

/// Create a description from a list of paragraphs.
///
/// Based on the Git CLI behavior. See `opt_parse_m()` and `cleanup_mode` in
/// `git/builtin/commit.c`.
pub fn join_message_paragraphs(paragraphs: &[String]) -> String {
    // Ensure each paragraph ends with a newline, then add another newline between
    // paragraphs.
    paragraphs
        .iter()
        .map(|p| text_util::complete_newline(p.as_str()))
        .join("\n")
}

pub fn description_template_for_describe(
    ui: &Ui,
    settings: &UserSettings,
    workspace_command: &WorkspaceCommandHelper,
    commit: &Commit,
) -> Result<String, CommandError> {
    let mut diff_summary_bytes = Vec::new();
    let diff_renderer = workspace_command.diff_renderer(vec![DiffFormat::Summary]);
    diff_renderer.show_patch(
        ui,
        &mut PlainTextFormatter::new(&mut diff_summary_bytes),
        commit,
        &EverythingMatcher,
    )?;
    let description = if commit.description().is_empty() {
        settings.default_description()
    } else {
        commit.description().to_owned()
    };
    if diff_summary_bytes.is_empty() {
        Ok(description)
    } else {
        Ok(description + "\n" + &diff_summary_to_description(&diff_summary_bytes))
    }
}

pub fn description_template_for_commit(
    ui: &Ui,
    settings: &UserSettings,
    workspace_command: &WorkspaceCommandHelper,
    intro: &str,
    overall_commit_description: &str,
    from_tree: &MergedTree,
    to_tree: &MergedTree,
) -> Result<String, CommandError> {
    let mut diff_summary_bytes = Vec::new();
    let diff_renderer = workspace_command.diff_renderer(vec![DiffFormat::Summary]);
    diff_renderer.show_diff(
        ui,
        &mut PlainTextFormatter::new(&mut diff_summary_bytes),
        from_tree,
        to_tree,
        &EverythingMatcher,
    )?;
    let mut template_chunks = Vec::new();
    if !intro.is_empty() {
        template_chunks.push(format!("JJ: {intro}\n"));
    }
    template_chunks.push(if overall_commit_description.is_empty() {
        settings.default_description()
    } else {
        overall_commit_description.to_owned()
    });
    if !diff_summary_bytes.is_empty() {
        template_chunks.push("\n".to_owned());
        template_chunks.push(diff_summary_to_description(&diff_summary_bytes));
    }
    Ok(template_chunks.concat())
}

pub fn diff_summary_to_description(bytes: &[u8]) -> String {
    let text = std::str::from_utf8(bytes).expect(
        "Summary diffs and repo paths must always be valid UTF8.",
        // Double-check this assumption for diffs that include file content.
    );
    "JJ: This commit contains the following changes:\n".to_owned()
        + &textwrap::indent(text, "JJ:     ")
}
