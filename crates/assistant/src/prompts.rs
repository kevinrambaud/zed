use crate::codegen::CodegenKind;
use gpui::AsyncAppContext;
use language::{BufferSnapshot, OffsetRangeExt, ToOffset};
use semantic_index::SearchResult;
use std::cmp::{self, Reverse};
use std::fmt::Write;
use std::ops::Range;
use std::path::PathBuf;
use tiktoken_rs::ChatCompletionRequestMessage;

pub struct PromptCodeSnippet {
    path: Option<PathBuf>,
    language_name: Option<String>,
    content: String,
}

impl PromptCodeSnippet {
    pub fn new(search_result: SearchResult, cx: &AsyncAppContext) -> Self {
        let (content, language_name, file_path) =
            search_result.buffer.read_with(cx, |buffer, _| {
                let snapshot = buffer.snapshot();
                let content = snapshot
                    .text_for_range(search_result.range.clone())
                    .collect::<String>();

                let language_name = buffer
                    .language()
                    .and_then(|language| Some(language.name().to_string()));

                let file_path = buffer
                    .file()
                    .and_then(|file| Some(file.path().to_path_buf()));

                (content, language_name, file_path)
            });

        PromptCodeSnippet {
            path: file_path,
            language_name,
            content,
        }
    }
}

impl ToString for PromptCodeSnippet {
    fn to_string(&self) -> String {
        let path = self
            .path
            .as_ref()
            .and_then(|path| Some(path.to_string_lossy().to_string()))
            .unwrap_or("".to_string());
        let language_name = self.language_name.clone().unwrap_or("".to_string());
        let content = self.content.clone();

        format!("The below code snippet may be relevant from file: {path}\n```{language_name}\n{content}\n```")
    }
}

#[allow(dead_code)]
fn summarize(buffer: &BufferSnapshot, selected_range: Range<impl ToOffset>) -> String {
    #[derive(Debug)]
    struct Match {
        collapse: Range<usize>,
        keep: Vec<Range<usize>>,
    }

    let selected_range = selected_range.to_offset(buffer);
    let mut ts_matches = buffer.matches(0..buffer.len(), |grammar| {
        Some(&grammar.embedding_config.as_ref()?.query)
    });
    let configs = ts_matches
        .grammars()
        .iter()
        .map(|g| g.embedding_config.as_ref().unwrap())
        .collect::<Vec<_>>();
    let mut matches = Vec::new();
    while let Some(mat) = ts_matches.peek() {
        let config = &configs[mat.grammar_index];
        if let Some(collapse) = mat.captures.iter().find_map(|cap| {
            if Some(cap.index) == config.collapse_capture_ix {
                Some(cap.node.byte_range())
            } else {
                None
            }
        }) {
            let mut keep = Vec::new();
            for capture in mat.captures.iter() {
                if Some(capture.index) == config.keep_capture_ix {
                    keep.push(capture.node.byte_range());
                } else {
                    continue;
                }
            }
            ts_matches.advance();
            matches.push(Match { collapse, keep });
        } else {
            ts_matches.advance();
        }
    }
    matches.sort_unstable_by_key(|mat| (mat.collapse.start, Reverse(mat.collapse.end)));
    let mut matches = matches.into_iter().peekable();

    let mut summary = String::new();
    let mut offset = 0;
    let mut flushed_selection = false;
    while let Some(mat) = matches.next() {
        // Keep extending the collapsed range if the next match surrounds
        // the current one.
        while let Some(next_mat) = matches.peek() {
            if mat.collapse.start <= next_mat.collapse.start
                && mat.collapse.end >= next_mat.collapse.end
            {
                matches.next().unwrap();
            } else {
                break;
            }
        }

        if offset > mat.collapse.start {
            // Skip collapsed nodes that have already been summarized.
            offset = cmp::max(offset, mat.collapse.end);
            continue;
        }

        if offset <= selected_range.start && selected_range.start <= mat.collapse.end {
            if !flushed_selection {
                // The collapsed node ends after the selection starts, so we'll flush the selection first.
                summary.extend(buffer.text_for_range(offset..selected_range.start));
                summary.push_str("<|START|");
                if selected_range.end == selected_range.start {
                    summary.push_str(">");
                } else {
                    summary.extend(buffer.text_for_range(selected_range.clone()));
                    summary.push_str("|END|>");
                }
                offset = selected_range.end;
                flushed_selection = true;
            }

            // If the selection intersects the collapsed node, we won't collapse it.
            if selected_range.end >= mat.collapse.start {
                continue;
            }
        }

        summary.extend(buffer.text_for_range(offset..mat.collapse.start));
        for keep in mat.keep {
            summary.extend(buffer.text_for_range(keep));
        }
        offset = mat.collapse.end;
    }

    // Flush selection if we haven't already done so.
    if !flushed_selection && offset <= selected_range.start {
        summary.extend(buffer.text_for_range(offset..selected_range.start));
        summary.push_str("<|START|");
        if selected_range.end == selected_range.start {
            summary.push_str(">");
        } else {
            summary.extend(buffer.text_for_range(selected_range.clone()));
            summary.push_str("|END|>");
        }
        offset = selected_range.end;
    }

    summary.extend(buffer.text_for_range(offset..buffer.len()));
    summary
}

pub fn generate_content_prompt(
    user_prompt: String,
    language_name: Option<&str>,
    buffer: &BufferSnapshot,
    range: Range<impl ToOffset>,
    kind: CodegenKind,
    search_results: Vec<PromptCodeSnippet>,
    model: &str,
) -> String {
    const MAXIMUM_SNIPPET_TOKEN_COUNT: usize = 500;
    const RESERVED_TOKENS_FOR_GENERATION: usize = 1000;

    let mut prompts = Vec::new();
    let range = range.to_offset(buffer);

    // General Preamble
    if let Some(language_name) = language_name {
        prompts.push(format!("You're an expert {language_name} engineer.\n"));
    } else {
        prompts.push("You're an expert engineer.\n".to_string());
    }

    // Snippets
    let mut snippet_position = prompts.len() - 1;

    let mut content = String::new();
    content.extend(buffer.text_for_range(0..range.start));
    if range.start == range.end {
        content.push_str("<|START|>");
    } else {
        content.push_str("<|START|");
    }
    content.extend(buffer.text_for_range(range.clone()));
    if range.start != range.end {
        content.push_str("|END|>");
    }
    content.extend(buffer.text_for_range(range.end..buffer.len()));

    prompts.push("The file you are currently working on has the following content:\n".to_string());

    if let Some(language_name) = language_name {
        let language_name = language_name.to_lowercase();
        prompts.push(format!("```{language_name}\n{content}\n```"));
    } else {
        prompts.push(format!("```\n{content}\n```"));
    }

    match kind {
        CodegenKind::Generate { position: _ } => {
            prompts.push("In particular, the user's cursor is currently on the '<|START|>' span in the above outline, with no text selected.".to_string());
            prompts
                .push("Assume the cursor is located where the `<|START|` marker is.".to_string());
            prompts.push(
                "Text can't be replaced, so assume your answer will be inserted at the cursor."
                    .to_string(),
            );
            prompts.push(format!(
                "Generate text based on the users prompt: {user_prompt}"
            ));
        }
        CodegenKind::Transform { range: _ } => {
            prompts.push("In particular, the user has selected a section of the text between the '<|START|' and '|END|>' spans.".to_string());
            prompts.push(format!(
                "Modify the users code selected text based upon the users prompt: '{user_prompt}'"
            ));
            prompts.push("You MUST reply with only the adjusted code (within the '<|START|' and '|END|>' spans), not the entire file.".to_string());
        }
    }

    if let Some(language_name) = language_name {
        prompts.push(format!(
            "Your answer MUST always and only be valid {language_name}"
        ));
    }
    prompts.push("Never make remarks about the output.".to_string());
    prompts.push("Do not return any text, except the generated code.".to_string());
    prompts.push("Do not wrap your text in a Markdown block".to_string());

    let current_messages = [ChatCompletionRequestMessage {
        role: "user".to_string(),
        content: Some(prompts.join("\n")),
        function_call: None,
        name: None,
    }];

    let mut remaining_token_count = if let Ok(current_token_count) =
        tiktoken_rs::num_tokens_from_messages(model, &current_messages)
    {
        let max_token_count = tiktoken_rs::model::get_context_size(model);
        let intermediate_token_count = max_token_count - current_token_count;

        if intermediate_token_count < RESERVED_TOKENS_FOR_GENERATION {
            0
        } else {
            intermediate_token_count - RESERVED_TOKENS_FOR_GENERATION
        }
    } else {
        // If tiktoken fails to count token count, assume we have no space remaining.
        0
    };

    // TODO:
    //   - add repository name to snippet
    //   - add file path
    //   - add language
    if let Ok(encoding) = tiktoken_rs::get_bpe_from_model(model) {
        let mut template = "You are working inside a large repository, here are a few code snippets that may be useful";

        for search_result in search_results {
            let mut snippet_prompt = template.to_string();
            let snippet = search_result.to_string();
            writeln!(snippet_prompt, "```\n{snippet}\n```").unwrap();

            let token_count = encoding
                .encode_with_special_tokens(snippet_prompt.as_str())
                .len();
            if token_count <= remaining_token_count {
                if token_count < MAXIMUM_SNIPPET_TOKEN_COUNT {
                    prompts.insert(snippet_position, snippet_prompt);
                    snippet_position += 1;
                    remaining_token_count -= token_count;
                    // If you have already added the template to the prompt, remove the template.
                    template = "";
                }
            } else {
                break;
            }
        }
    }

    prompts.join("\n")
}

#[cfg(test)]
pub(crate) mod tests {

    use super::*;
    use std::sync::Arc;

    use gpui::AppContext;
    use indoc::indoc;
    use language::{language_settings, tree_sitter_rust, Buffer, Language, LanguageConfig, Point};
    use settings::SettingsStore;

    pub(crate) fn rust_lang() -> Language {
        Language::new(
            LanguageConfig {
                name: "Rust".into(),
                path_suffixes: vec!["rs".to_string()],
                ..Default::default()
            },
            Some(tree_sitter_rust::language()),
        )
        .with_embedding_query(
            r#"
            (
                [(line_comment) (attribute_item)]* @context
                .
                [
                    (struct_item
                        name: (_) @name)

                    (enum_item
                        name: (_) @name)

                    (impl_item
                        trait: (_)? @name
                        "for"? @name
                        type: (_) @name)

                    (trait_item
                        name: (_) @name)

                    (function_item
                        name: (_) @name
                        body: (block
                            "{" @keep
                            "}" @keep) @collapse)

                    (macro_definition
                        name: (_) @name)
                    ] @item
                )
            "#,
        )
        .unwrap()
    }

    #[gpui::test]
    fn test_outline_for_prompt(cx: &mut AppContext) {
        cx.set_global(SettingsStore::test(cx));
        language_settings::init(cx);
        let text = indoc! {"
            struct X {
                a: usize,
                b: usize,
            }

            impl X {

                fn new() -> Self {
                    let a = 1;
                    let b = 2;
                    Self { a, b }
                }

                pub fn a(&self, param: bool) -> usize {
                    self.a
                }

                pub fn b(&self) -> usize {
                    self.b
                }
            }
        "};
        let buffer =
            cx.add_model(|cx| Buffer::new(0, 0, text).with_language(Arc::new(rust_lang()), cx));
        let snapshot = buffer.read(cx).snapshot();

        assert_eq!(
            summarize(&snapshot, Point::new(1, 4)..Point::new(1, 4)),
            indoc! {"
                struct X {
                    <|START|>a: usize,
                    b: usize,
                }

                impl X {

                    fn new() -> Self {}

                    pub fn a(&self, param: bool) -> usize {}

                    pub fn b(&self) -> usize {}
                }
            "}
        );

        assert_eq!(
            summarize(&snapshot, Point::new(8, 12)..Point::new(8, 14)),
            indoc! {"
                struct X {
                    a: usize,
                    b: usize,
                }

                impl X {

                    fn new() -> Self {
                        let <|START|a |END|>= 1;
                        let b = 2;
                        Self { a, b }
                    }

                    pub fn a(&self, param: bool) -> usize {}

                    pub fn b(&self) -> usize {}
                }
            "}
        );

        assert_eq!(
            summarize(&snapshot, Point::new(6, 0)..Point::new(6, 0)),
            indoc! {"
                struct X {
                    a: usize,
                    b: usize,
                }

                impl X {
                <|START|>
                    fn new() -> Self {}

                    pub fn a(&self, param: bool) -> usize {}

                    pub fn b(&self) -> usize {}
                }
            "}
        );

        assert_eq!(
            summarize(&snapshot, Point::new(21, 0)..Point::new(21, 0)),
            indoc! {"
                struct X {
                    a: usize,
                    b: usize,
                }

                impl X {

                    fn new() -> Self {}

                    pub fn a(&self, param: bool) -> usize {}

                    pub fn b(&self) -> usize {}
                }
                <|START|>"}
        );

        // Ensure nested functions get collapsed properly.
        let text = indoc! {"
            struct X {
                a: usize,
                b: usize,
            }

            impl X {

                fn new() -> Self {
                    let a = 1;
                    let b = 2;
                    Self { a, b }
                }

                pub fn a(&self, param: bool) -> usize {
                    let a = 30;
                    fn nested() -> usize {
                        3
                    }
                    self.a + nested()
                }

                pub fn b(&self) -> usize {
                    self.b
                }
            }
        "};
        buffer.update(cx, |buffer, cx| buffer.set_text(text, cx));
        let snapshot = buffer.read(cx).snapshot();
        assert_eq!(
            summarize(&snapshot, Point::new(0, 0)..Point::new(0, 0)),
            indoc! {"
                <|START|>struct X {
                    a: usize,
                    b: usize,
                }

                impl X {

                    fn new() -> Self {}

                    pub fn a(&self, param: bool) -> usize {}

                    pub fn b(&self) -> usize {}
                }
            "}
        );
    }
}