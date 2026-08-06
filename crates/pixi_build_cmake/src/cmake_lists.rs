//! Reads what a CMake project declares in its `project()` call.
//!
//! CMake requires the top level `CMakeLists.txt` to call `project()`, and that
//! call declares the languages the build needs along with the version,
//! description and homepage of the project. Reading it tells the backend which
//! compilers to add to the build requirements instead of assuming a C++
//! project, and provides package metadata the manifest leaves out.

/// What the `project()` call of a CMake project declares.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProjectDeclaration {
    /// The languages the project enables. Empty when it enables none, which
    /// `project(name NONE)` and `project(name LANGUAGES NONE)` both express.
    pub languages: Vec<String>,
    /// The value of the `VERSION` keyword.
    pub version: Option<String>,
    /// The value of the `DESCRIPTION` keyword.
    pub description: Option<String>,
    /// The value of the `HOMEPAGE_URL` keyword.
    pub homepage_url: Option<String>,
}

/// Returns what the `project()` call in `cmake_lists` declares, or `None` if
/// the file contains no `project()` call.
pub fn parse_project(cmake_lists: &str) -> Option<ProjectDeclaration> {
    // A byte order mark would hide the first line from the line-based search.
    let cmake_lists = cmake_lists.strip_prefix('\u{feff}').unwrap_or(cmake_lists);
    let arguments = project_arguments(&strip_comments(cmake_lists))?;

    // The first argument is the project name, the rest describes the project.
    let mut arguments = arguments.iter().skip(1);
    let mut declaration = ProjectDeclaration::default();
    let mut declared_languages = false;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "VERSION" => declaration.version = arguments.next().cloned(),
            "DESCRIPTION" => declaration.description = arguments.next().cloned(),
            "HOMEPAGE_URL" => declaration.homepage_url = arguments.next().cloned(),
            "LANGUAGES" => declared_languages = true,
            language => {
                declared_languages = true;
                declaration.languages.push(language.to_string());
            }
        }
    }

    // `NONE` disables every language, both on its own and in a language list.
    if declaration
        .languages
        .iter()
        .any(|language| language == "NONE")
    {
        declaration.languages.clear();
    } else if !declared_languages {
        // CMake enables C and CXX when the call names no language.
        declaration.languages = vec!["C".to_string(), "CXX".to_string()];
    }

    Some(declaration)
}

/// Returns the compilers needed for `languages`, in the order the languages
/// were declared and without duplicates.
///
/// Languages that conda-forge has no compiler package for are skipped; their
/// compiler comes from the compilers of the languages that are mapped.
pub fn compilers_for_languages(languages: &[String]) -> Vec<String> {
    let mut compilers: Vec<String> = Vec::new();

    for language in languages {
        let Some(compiler) = compiler_for_language(language) else {
            tracing::debug!("no compiler known for CMake language `{language}`, skipping it");
            continue;
        };
        if !compilers.iter().any(|known| known == compiler) {
            compilers.push(compiler.to_string());
        }
    }

    compilers
}

/// Maps a CMake language to the compiler that builds it.
fn compiler_for_language(language: &str) -> Option<&'static str> {
    match language {
        // Assembly and Objective-C are built by the C and C++ compilers.
        "C" | "ASM" | "OBJC" => Some("c"),
        "CXX" | "OBJCXX" => Some("cxx"),
        "CUDA" => Some("cuda"),
        "Fortran" => Some("fortran"),
        _ => None,
    }
}

/// Returns the arguments of the first `project()` call in `cmake_lists`.
fn project_arguments(cmake_lists: &str) -> Option<Vec<String>> {
    // Lowercasing per ASCII keeps the offsets identical to the original, so
    // the copy and the quote map are built once and shared by every search.
    let lowercased = cmake_lists.to_ascii_lowercase();
    let quoted = quoted_regions(&lowercased);
    let mut search_from = 0;

    while let Some(offset) = find_command(&lowercased, &quoted, search_from, "project") {
        match split_arguments(&cmake_lists[offset..]) {
            Some(arguments) if !arguments.is_empty() => return Some(arguments),
            // A `project()` call without a name is invalid CMake, keep looking.
            _ => search_from = offset,
        }
    }

    None
}

/// Returns the offset just past the opening parenthesis of the next `name`
/// command at or after `search_from`, given the file lowercased per ASCII and
/// the map of its quoted bytes.
///
/// Command names are case insensitive and may be separated from their opening
/// parenthesis by whitespace.
fn find_command(
    lowercased: &str,
    quoted: &[bool],
    mut search_from: usize,
    name: &str,
) -> Option<usize> {
    while let Some(offset) = lowercased[search_from..].find(name) {
        let start = search_from + offset;
        let end = start + name.len();
        search_from = end;

        // A command name inside a string argument is just text.
        if quoted[start] {
            continue;
        }

        // A command invocation only ever starts a line, so anything but
        // whitespace before the name on its line makes it argument text.
        let at_line_start = lowercased[..start]
            .chars()
            .rev()
            .take_while(|&character| character != '\n')
            .all(char::is_whitespace);
        if !at_line_start {
            continue;
        }

        let mut remainder = lowercased[end..]
            .char_indices()
            .skip_while(|(_, character)| character.is_whitespace());
        if let Some((offset, '(')) = remainder.next() {
            return Some(end + offset + 1);
        }
    }

    None
}

/// Marks every byte that sits inside a quoted or bracket argument.
///
/// Used to keep a command name that appears inside a string or a bracket
/// argument, such as a help text mentioning `project(...)`, from being read
/// as a call.
fn quoted_regions(cmake_lists: &str) -> Vec<bool> {
    let mut quoted = vec![false; cmake_lists.len()];
    let mut inside = false;
    let mut escaped = false;
    let mut offset = 0;

    while let Some(character) = cmake_lists[offset..].chars().next() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            inside = !inside;
        } else if character == '['
            && !inside
            && let Some(length) = bracket_argument_length(&cmake_lists[offset..])
        {
            for flag in &mut quoted[offset..offset + length] {
                *flag = true;
            }
            offset += length;
            continue;
        }

        for flag in &mut quoted[offset..offset + character.len_utf8()] {
            *flag = inside;
        }
        offset += character.len_utf8();
    }

    quoted
}

/// Returns the length of the bracket argument opening at the start of `text`,
/// terminator included and running to the end when there is no terminator, or
/// `None` when no bracket argument opens here.
fn bracket_argument_length(text: &str) -> Option<usize> {
    let mut characters = text.chars();
    if characters.next() != Some('[') {
        return None;
    }
    let mut equals_signs = 0;
    loop {
        match characters.next() {
            Some('=') => equals_signs += 1,
            Some('[') => break,
            _ => return None,
        }
    }

    let opening = 2 + equals_signs;
    let terminator = format!("]{}]", "=".repeat(equals_signs));
    let length = text[opening..]
        .find(&terminator)
        .map_or(text.len(), |position| opening + position + terminator.len());
    Some(length)
}

/// Splits the arguments of a command whose opening parenthesis was already
/// consumed, stopping at the matching closing parenthesis.
fn split_arguments(arguments: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut escaped = false;

    for character in arguments.chars() {
        if escaped {
            // CMake's encoded escapes; every other escape means the character.
            token.push(match character {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            // A quoted argument is a token even when it is empty.
            quoted = !quoted;
            if !quoted {
                tokens.push(std::mem::take(&mut token));
            }
        } else if quoted {
            token.push(character);
        } else if character == ';' {
            // An unquoted semicolon separates the elements of a CMake list.
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else if character.is_whitespace() || character == ')' {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
            if character == ')' {
                return Some(tokens);
            }
        } else {
            token.push(character);
        }
    }

    // The command is not closed, so the file is not valid CMake.
    None
}

/// Removes line comments and bracket comments, leaving quoted arguments,
/// bracket arguments and the overall line structure intact.
fn strip_comments(cmake_lists: &str) -> String {
    let mut stripped = String::with_capacity(cmake_lists.len());
    let mut characters = cmake_lists.chars().peekable();
    let mut quoted = false;
    let mut escaped = false;

    while let Some(character) = characters.next() {
        if escaped {
            stripped.push(character);
            escaped = false;
        } else if character == '\\' {
            stripped.push(character);
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
            stripped.push(character);
        } else if character == '#' && !quoted {
            match take_bracket_marker(&mut characters) {
                Some(equals_signs) => {
                    take_bracket_body(&mut characters, equals_signs);
                }
                None => {
                    while characters.peek().is_some_and(|&next| next != '\n') {
                        characters.next();
                    }
                }
            }
        } else if character == '[' && !quoted {
            // A bracket argument is raw text, so it is copied verbatim.
            stripped.push(character);
            if let Some(equals_signs) = take_bracket_marker_rest(&mut characters) {
                stripped.push_str(&"=".repeat(equals_signs));
                stripped.push('[');
                stripped.push_str(&take_bracket_body(&mut characters, equals_signs));
            }
        } else {
            stripped.push(character);
        }
    }

    stripped
}

/// Consumes the `[=*[` opening of a bracket comment and returns the number of
/// equals signs it uses, leaving the iterator untouched for a line comment.
fn take_bracket_marker(characters: &mut std::iter::Peekable<std::str::Chars>) -> Option<usize> {
    if characters.peek() != Some(&'[') {
        return None;
    }

    let mut lookahead = characters.clone();
    lookahead.next();
    let equals_signs = take_bracket_marker_rest(&mut lookahead)?;
    *characters = lookahead;
    Some(equals_signs)
}

/// Consumes the `=*[` remainder of a bracket marker whose opening `[` was
/// already consumed and returns the number of equals signs it uses, leaving
/// the iterator untouched when no marker follows.
fn take_bracket_marker_rest(
    characters: &mut std::iter::Peekable<std::str::Chars>,
) -> Option<usize> {
    // Only commit once the whole marker is present, so scan a copy first.
    let mut lookahead = characters.clone();
    let mut equals_signs = 0;
    loop {
        match lookahead.next() {
            Some('=') => equals_signs += 1,
            Some('[') => break,
            _ => return None,
        }
    }

    *characters = lookahead;
    Some(equals_signs)
}

/// Consumes a bracket's body up to and including its `]=*]` terminator and
/// returns everything consumed.
fn take_bracket_body(
    characters: &mut std::iter::Peekable<std::str::Chars>,
    equals_signs: usize,
) -> String {
    let terminator = format!("]{}]", "=".repeat(equals_signs));
    let mut body = String::new();

    for character in characters {
        body.push(character);
        if body.ends_with(&terminator) {
            break;
        }
    }

    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn languages(cmake_lists: &str) -> Option<Vec<String>> {
        parse_project(cmake_lists).map(|declaration| declaration.languages)
    }

    #[test]
    fn test_languages_default_to_c_and_cxx() {
        assert_eq!(
            languages("project(demo)"),
            Some(vec!["C".to_string(), "CXX".to_string()])
        );
    }

    #[test]
    fn test_explicit_language_list() {
        assert_eq!(
            languages("project(demo LANGUAGES CXX CUDA)"),
            Some(vec!["CXX".to_string(), "CUDA".to_string()])
        );
    }

    #[test]
    fn test_keywords_with_values_are_skipped() {
        assert_eq!(
            languages(
                r#"project(demo VERSION 1.2.3 DESCRIPTION "a demo" HOMEPAGE_URL https://example.com LANGUAGES C)"#
            ),
            Some(vec!["C".to_string()])
        );
    }

    #[test]
    fn test_legacy_languages_without_keyword() {
        assert_eq!(
            languages("project(demo C CXX Fortran)"),
            Some(vec![
                "C".to_string(),
                "CXX".to_string(),
                "Fortran".to_string()
            ])
        );
    }

    #[test]
    fn test_none_disables_all_languages() {
        assert_eq!(languages("project(demo NONE)"), Some(Vec::new()));
        assert_eq!(languages("project(demo LANGUAGES NONE)"), Some(Vec::new()));
    }

    #[test]
    fn test_multiline_call_with_comments() {
        let cmake_lists = r#"
cmake_minimum_required(VERSION 3.25)
# project(ignored LANGUAGES Fortran)
project(
    demo # the name
    VERSION 1.0
    LANGUAGES
        C
        CXX
)
"#;
        assert_eq!(
            languages(cmake_lists),
            Some(vec!["C".to_string(), "CXX".to_string()])
        );
    }

    #[test]
    fn test_bracket_comment_is_ignored() {
        let cmake_lists = r#"
#[==[
project(ignored LANGUAGES Fortran)
]==]
project(demo LANGUAGES CXX)
"#;
        assert_eq!(languages(cmake_lists), Some(vec!["CXX".to_string()]));
    }

    #[test]
    fn test_command_name_is_case_insensitive_and_may_be_spaced() {
        assert_eq!(
            languages("PROJECT (demo LANGUAGES C)"),
            Some(vec!["C".to_string()])
        );
    }

    #[test]
    fn test_identifiers_ending_in_project_are_not_commands() {
        assert_eq!(
            languages("my_project(demo LANGUAGES Fortran)\nproject(demo LANGUAGES C)"),
            Some(vec!["C".to_string()])
        );
    }

    #[test]
    fn test_quoted_name_and_languages() {
        assert_eq!(
            languages(r#"project("demo" LANGUAGES "CXX")"#),
            Some(vec!["CXX".to_string()])
        );
    }

    /// A command name inside a string argument, such as a help text, is not
    /// a call.
    #[test]
    fn test_call_inside_a_string_is_ignored() {
        let cmake_lists =
            "set(HELP \"run project(fake LANGUAGES Fortran) first\")\nproject(demo LANGUAGES C)\n";

        assert_eq!(languages(cmake_lists), Some(vec!["C".to_string()]));
    }

    /// An escaped quote in an unquoted argument does not open a string, so
    /// the comment after it is still a comment.
    #[test]
    fn test_escaped_quote_in_an_unquoted_argument() {
        let cmake_lists = concat!(
            r#"add_definitions(-DGREETING=\"hello\")"#,
            "\n",
            "# project(fake LANGUAGES Fortran)\n",
            "project(demo LANGUAGES C)\n"
        );

        assert_eq!(languages(cmake_lists), Some(vec!["C".to_string()]));
    }

    /// A bracket argument is raw text, so a `project()` mentioned inside one
    /// is not a call.
    #[test]
    fn test_call_inside_a_bracket_argument_is_ignored() {
        let cmake_lists = "message([[project(fake LANGUAGES Fortran)]])\nproject(demo LANGUAGES C)";

        assert_eq!(languages(cmake_lists), Some(vec!["C".to_string()]));

        let cmake_lists = "message([=[project(fake) and ]] too]=])\nproject(demo LANGUAGES C)";

        assert_eq!(languages(cmake_lists), Some(vec!["C".to_string()]));
    }

    /// A lone quote or comment character inside a bracket argument is raw
    /// text and must not affect the rest of the file.
    #[test]
    fn test_bracket_argument_content_does_not_leak() {
        let cmake_lists = "set(X [[say \"hello]])\nproject(demo LANGUAGES C)";

        assert_eq!(languages(cmake_lists), Some(vec!["C".to_string()]));

        let cmake_lists = "set(X [[a # b \"]])\nproject(demo LANGUAGES C)";

        assert_eq!(languages(cmake_lists), Some(vec!["C".to_string()]));
    }

    /// An unquoted `C;CXX` is a CMake list holding two languages, while a
    /// quoted or escaped semicolon is just a character.
    #[test]
    fn test_semicolon_separated_language_list() {
        assert_eq!(
            languages("project(demo LANGUAGES C;CXX)"),
            Some(vec!["C".to_string(), "CXX".to_string()])
        );

        let declaration = parse_project(r#"project(demo DESCRIPTION "a;b" LANGUAGES C)"#)
            .expect("the call should be found");
        assert_eq!(declaration.description.as_deref(), Some("a;b"));
    }

    /// CMake reads `\n` as a newline and `\"` as a quote, not as `n` and `"`.
    #[test]
    fn test_escape_sequences_are_decoded() {
        let declaration =
            parse_project(r#"project(demo DESCRIPTION "one\ntwo \"quoted\"" LANGUAGES C)"#)
                .expect("the call should be found");

        assert_eq!(
            declaration.description.as_deref(),
            Some("one\ntwo \"quoted\"")
        );
    }

    /// A command invocation only ever starts a line, so a `project(` inside
    /// another call's arguments is argument text.
    #[test]
    fn test_call_inside_another_calls_arguments_is_ignored() {
        let cmake_lists = "set(P C:\\project(x))\nproject(demo LANGUAGES C)";

        assert_eq!(languages(cmake_lists), Some(vec!["C".to_string()]));

        let cmake_lists = "cmake_parse_arguments(project (x))\nproject(demo LANGUAGES C)";

        assert_eq!(languages(cmake_lists), Some(vec!["C".to_string()]));
    }

    /// A byte order mark does not keep the first line from holding a call.
    #[test]
    fn test_byte_order_mark_is_skipped() {
        assert_eq!(
            languages("\u{feff}project(demo LANGUAGES C)"),
            Some(vec!["C".to_string()])
        );
    }

    #[test]
    fn test_without_project_call() {
        assert_eq!(languages("add_subdirectory(sub)"), None);
        assert_eq!(languages(""), None);
    }

    #[test]
    fn test_unterminated_call() {
        assert_eq!(languages("project(demo LANGUAGES C"), None);
    }

    #[test]
    fn test_metadata_keywords() {
        let declaration = parse_project(
            r#"project(demo VERSION 1.2.3 DESCRIPTION "a demo" HOMEPAGE_URL "https://example.com")"#,
        )
        .expect("the call should be found");

        assert_eq!(declaration.version.as_deref(), Some("1.2.3"));
        assert_eq!(declaration.description.as_deref(), Some("a demo"));
        assert_eq!(
            declaration.homepage_url.as_deref(),
            Some("https://example.com")
        );
        // Metadata keywords say nothing about the languages.
        assert_eq!(
            declaration.languages,
            vec!["C".to_string(), "CXX".to_string()]
        );
    }

    #[test]
    fn test_metadata_keywords_are_absent_by_default() {
        let declaration = parse_project("project(demo)").expect("the call should be found");

        assert_eq!(declaration.version, None);
        assert_eq!(declaration.description, None);
        assert_eq!(declaration.homepage_url, None);
    }

    /// A keyword at the very end of the call has no value to take.
    #[test]
    fn test_metadata_keyword_without_a_value() {
        let declaration = parse_project("project(demo VERSION)").expect("the call should be found");

        assert_eq!(declaration.version, None);
    }

    #[test]
    fn test_compilers_are_mapped_and_deduplicated() {
        let languages = ["C", "ASM", "CXX", "OBJCXX", "CUDA", "Fortran", "Swift"];
        let languages: Vec<String> = languages.iter().map(|l| (*l).to_string()).collect();

        assert_eq!(
            compilers_for_languages(&languages),
            vec![
                "c".to_string(),
                "cxx".to_string(),
                "cuda".to_string(),
                "fortran".to_string()
            ]
        );
    }

    #[test]
    fn test_compilers_for_no_languages() {
        assert!(compilers_for_languages(&[]).is_empty());
    }
}
