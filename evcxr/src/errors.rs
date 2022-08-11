// Copyright 2020 The Evcxr Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::code_block::count_columns;
use crate::code_block::CodeBlock;
use crate::code_block::CodeKind;
use crate::code_block::CommandCall;
use crate::code_block::Segment;
use crate::code_block::UserCodeInfo;
use json::JsonValue;
use json::{self};
use once_cell::sync::OnceCell;
use ra_ap_ide::TextRange;
use ra_ap_ide::TextSize;
use regex::Regex;
use std::fmt;
use std::io;

#[derive(Debug, Clone)]
pub struct CompilationError {
    message: String,
    pub json: JsonValue,
    pub(crate) code_origins: Vec<CodeKind>,
    spanned_messages: Vec<SpannedMessage>,
    level: String,
}

fn spans_in_local_source(span: &JsonValue) -> Option<&JsonValue> {
    if let Some(file_name) = span["file_name"].as_str() {
        if file_name.ends_with("lib.rs") {
            return Some(span);
        }
    }
    let expansion = &span["expansion"];
    if expansion.is_object() {
        return spans_in_local_source(&expansion["span"]);
    }
    None
}

fn get_code_origins_for_span<'a>(
    span: &JsonValue,
    code_block: &'a CodeBlock,
) -> (Vec<(&'a CodeKind, usize)>, (usize, usize)) {
    if let Some(span) = spans_in_local_source(span) {
        let mut code_origins = Vec::new();

        if let (Some(line_start), Some(line_end)) =
            (span["line_start"].as_usize(), span["line_end"].as_usize())
        {
            for line in line_start..=line_end {
                code_origins.push(code_block.origin_for_line(line));
            }
        }
        let mut bs = span["byte_start"].as_usize().unwrap_or(0) + 20;
        let mut be = span["byte_end"].as_usize().unwrap_or(0) + 20;
        for x in &code_block.segments {
            if x.code.len() > bs {
                break;
            }
            if matches!(x.kind, CodeKind::OriginalUserCode(_)) {
                break;
            }
            bs -= x.code.len();
        }
        for x in &code_block.segments {
            if x.code.len() > be {
                break;
            }
            if matches!(x.kind, CodeKind::OriginalUserCode(_)) {
                break;
            }
            be -= x.code.len();
        }
        (code_origins, (bs, be))
    } else {
        (vec![], (0, 0))
    }
}

fn get_code_origins<'a>(json: &JsonValue, code_block: &'a CodeBlock) -> Vec<&'a CodeKind> {
    let mut code_origins = Vec::new();
    if let JsonValue::Array(spans) = &json["spans"] {
        for span in spans {
            code_origins.extend(
                get_code_origins_for_span(span, code_block)
                    .0
                    .iter()
                    .map(|(origin, _)| origin),
            );
        }
    }
    code_origins
}

impl CompilationError {
    pub(crate) fn opt_new(mut json: JsonValue, code_block: &CodeBlock) -> Option<CompilationError> {
        // From Cargo 1.36 onwards, errors emitted as JSON get wrapped by Cargo.
        // Retrive the inner message emitted by the compiler.
        if json["message"].is_object() {
            json = json["message"].clone();
        }
        let mut code_origins = get_code_origins(&json, code_block);
        let mut user_error_json = None;
        if let JsonValue::Array(children) = &json["children"] {
            for child in children {
                let child_origins = get_code_origins(child, code_block);
                if !code_origins.iter().any(|k| k.is_user_supplied())
                    && child_origins.iter().any(|k| k.is_user_supplied())
                {
                    // Use the child instead of the top-level error.
                    user_error_json = Some(child.clone());
                    code_origins = child_origins;
                    break;
                } else {
                    code_origins.extend(child_origins);
                }
            }
        }
        if let Some(user_error_json) = user_error_json {
            json = user_error_json;
        }

        let message = if let Some(message) = json["message"].as_str() {
            if message.starts_with("aborting due to")
                || message.starts_with("For more information about")
                || message.starts_with("Some errors occurred")
            {
                return None;
            }
            sanitize_message(message)
        } else {
            return None;
        };

        Some(CompilationError {
            spanned_messages: build_spanned_messages(&json, code_block),
            message,
            level: json["level"].as_str().unwrap_or("").to_owned(),
            json,
            code_origins: code_origins.into_iter().cloned().collect(),
        })
    }

    pub(crate) fn fill_lines(&mut self, code_info: &UserCodeInfo) {
        for spanned_message in self.spanned_messages.iter_mut() {
            if let Some(span) = &spanned_message.span {
                spanned_message.lines.extend(
                    code_info.original_lines[span.start_line - 1..span.end_line]
                        .iter()
                        .map(|line| (*line).to_owned()),
                );
            }
        }
    }

    /// Returns a synthesized error that spans the specified portion of `segment`.
    pub(crate) fn from_segment_span(
        segment: &Segment,
        spanned_message: SpannedMessage,
        message: String,
    ) -> CompilationError {
        CompilationError {
            spanned_messages: vec![spanned_message],
            message,
            json: JsonValue::Null,
            code_origins: vec![segment.kind.clone()],
            level: "error".to_owned(),
        }
    }

    /// Returns whether this error originated in code supplied by the user.
    pub fn is_from_user_code(&self) -> bool {
        self.code_origins.iter().any(CodeKind::is_user_supplied)
    }

    /// Returns whether this error originated in code that we generated.
    pub fn is_from_generated_code(&self) -> bool {
        self.code_origins.contains(&CodeKind::OtherGeneratedCode)
    }

    pub fn message(&self) -> String {
        self.message.clone()
    }

    pub fn code(&self) -> Option<&str> {
        if let JsonValue::Object(code) = &self.json["code"] {
            return code["code"].as_str();
        }
        None
    }

    pub fn explanation(&self) -> Option<&str> {
        if let JsonValue::Object(code) = &self.json["code"] {
            return code["explanation"].as_str();
        }
        None
    }

    pub fn evcxr_extra_hint(&self) -> Option<&'static str> {
        if let Some(code) = self.code() {
            Some(match code {
                "E0597" => {
                    "Values assigned to variables in Evcxr cannot contain references \
                     (unless they're static)"
                }
                _ => return None,
            })
        } else {
            None
        }
    }

    pub fn spanned_messages(&self) -> &[SpannedMessage] {
        &self.spanned_messages[..]
    }

    /// Returns the primary spanned message, or if there is no primary spanned message, perhaps
    /// because it was reported in generated code, so go filtered out, then returns the first
    /// spanned message, if any.
    pub fn primary_spanned_message(&self) -> Option<&SpannedMessage> {
        match self.spanned_messages.iter().find(|msg| msg.is_primary) {
            Some(x) => Some(x),
            None => self.spanned_messages().first(),
        }
    }

    pub fn level(&self) -> &str {
        &self.level
    }

    pub fn help(&self) -> Vec<String> {
        if let JsonValue::Array(children) = &self.json["children"] {
            children
                .iter()
                .filter_map(|child| {
                    if child["level"].as_str() != Some("help") {
                        return None;
                    }
                    child["message"].as_str().map(|s| {
                        let mut message = s.to_owned();
                        if let Some(replacement) =
                            child["spans"][0]["suggested_replacement"].as_str()
                        {
                            use std::fmt::Write;
                            write!(message, "\n\n{}", replacement.trim_end()).unwrap();
                        }
                        message
                    })
                })
                .collect()
        } else {
            vec![]
        }
    }

    pub fn rendered(&self) -> String {
        self.json["rendered"].as_str().unwrap_or("").to_owned()
    }

    /// Returns the actual type indicated by the error message or None if this isn't a type error.
    pub(crate) fn get_actual_type(&self) -> Option<String> {
        // Observed formats:
        // Up to 1.40:
        //   message.children[].message
        //     "expected type `std::string::String`\n   found type `{integer}`"
        // 1.41+:
        //   message.children[].message
        //     "expected struct `std::string::String`\n     found enum `std::option::Option<std::string::String>`"
        //     "expected struct `std::string::String`\n    found tuple `({integer}, {float})`"
        //     "  expected struct `std::string::String`\nfound opaque type `impl Bar`"
        //   message.spans[].label
        //     "expected struct `std::string::String`, found integer"
        //     "expected struct `std::string::String`, found `i32`"
        static TYPE_ERROR_RE: OnceCell<Regex> = OnceCell::new();
        let type_error_re =
            TYPE_ERROR_RE.get_or_init(|| Regex::new(" *expected (?s:.)*found.* `(.*)`").unwrap());
        if let JsonValue::Array(children) = &self.json["children"] {
            for child in children {
                if let Some(message) = child["message"].as_str() {
                    if let Some(captures) = type_error_re.captures(message) {
                        return Some(captures[1].to_owned());
                    }
                }
            }
        }
        static TYPE_ERROR_RE2: OnceCell<Regex> = OnceCell::new();
        let type_error_re2 =
            TYPE_ERROR_RE2.get_or_init(|| Regex::new("expected .* found (integer|float)").unwrap());
        if let JsonValue::Array(spans) = &self.json["spans"] {
            for span in spans {
                if let Some(label) = span["label"].as_str() {
                    if let Some(captures) = type_error_re.captures(label) {
                        return Some(captures[1].to_owned());
                    } else if let Some(captures) = type_error_re2.captures(label) {
                        return Some(captures[1].to_owned());
                    }
                }
            }
        }
        None
    }
}

fn sanitize_message(message: &str) -> String {
    // Any references to `evcxr_variable_store` are beyond the end of what the
    // user typed, so we replace such references with something more meaningful.
    // This is mostly helpful with missing semicolons on let statements, which
    // produce errors such as "expected `;`, found `evcxr_variable_store`"
    message.replace("`evcxr_variable_store`", "<end of input>")
}

fn build_spanned_messages(json: &JsonValue, code_block: &CodeBlock) -> Vec<SpannedMessage> {
    let mut output_spans = Vec::new();
    if let JsonValue::Array(spans) = &json["spans"] {
        for span_json in spans {
            output_spans.push(SpannedMessage::from_json(span_json, code_block));
        }
    }
    if output_spans.iter().any(|s| s.span.is_some()) {
        // If we have at least one span in the user's code, remove all spans in generated
        // code. They'll be messages like "borrowed value only lives until here", which doesn't make
        // sense to show to the user, since "here" is is code that they didn't write and can't see.
        output_spans.retain(|s| s.span.is_some());
    }
    output_spans
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct Span {
    /// 1-based line number in the original user code on which the span starts (inclusive).
    pub start_line: usize,
    /// 1-based column (character) number in the original user code on which the span starts
    /// (inclusive).
    pub start_column: usize,
    /// 1-based line number in the original user code on which the span ends (inclusive).
    pub end_line: usize,
    /// 1-based column (character) number in the original user code on which the span ends
    /// (exclusive).
    pub end_column: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub code_block_id: usize,
}

impl Span {
    pub(crate) fn from_command(
        command: &CommandCall,
        start_column: usize,
        end_column: usize,
    ) -> Span {
        Span {
            start_line: command.line_number,
            start_column,
            end_line: command.line_number,
            end_column,
            byte_start: start_column,
            byte_end: end_column,
            code_block_id: 0,
        }
    }

    pub(crate) fn from_segment(segment: &Segment, range: TextRange) -> Option<Span> {
        if let CodeKind::OriginalUserCode(meta) = &segment.kind {
            let (start_line, start_column) = line_and_column(
                &segment.code,
                range.start(),
                meta.column_offset,
                meta.start_line,
            );
            let (end_line, end_column) = line_and_column(
                &segment.code,
                range.end(),
                meta.column_offset,
                meta.start_line,
            );
            Some(Span {
                start_line,
                start_column,
                end_line,
                end_column,
                byte_start: start_column,
                byte_end: end_column,
                code_block_id: 0,
            })
        } else {
            None
        }
    }
}

/// Returns the line and column number of `position` within `text`. Line and column numbers are
/// 1-based.
fn line_and_column(
    text: &str,
    position: TextSize,
    first_line_column_offset: usize,
    start_line: usize,
) -> (usize, usize) {
    let text = &text[..usize::from(position)];
    let line = text.lines().count();
    let mut column = text.lines().last().map(count_columns).unwrap_or(0) + 1;
    if line == 1 {
        column += first_line_column_offset;
    }
    (start_line + line - 1, column)
}

#[derive(Debug, Clone)]
pub struct SpannedMessage {
    pub span: Option<Span>,
    /// Output lines relevant to the message.
    pub lines: Vec<String>,
    pub label: String,
    pub is_primary: bool,
}

impl SpannedMessage {
    fn from_json(span_json: &JsonValue, code_block: &CodeBlock) -> SpannedMessage {
        let span = if let (Some(file_name), Some(start_column), Some(end_column)) = (
            span_json["file_name"].as_str(),
            span_json["column_start"].as_usize(),
            span_json["column_end"].as_usize(),
        ) {
            if file_name.ends_with("lib.rs") {
                let (origins, (bs, be)) = get_code_origins_for_span(span_json, code_block);
                if let (
                    Some((CodeKind::OriginalUserCode(start), start_line_offset)),
                    Some((CodeKind::OriginalUserCode(end), end_line_offset)),
                ) = (origins.first(), origins.last())
                {
                    Some(Span {
                        start_line: start.start_line + start_line_offset,
                        start_column: start_column
                            + (if *start_line_offset == 0 {
                                start.column_offset
                            } else {
                                0
                            }),
                        end_line: end.start_line + end_line_offset,
                        end_column: end_column
                            + (if *end_line_offset == 0 {
                                end.column_offset
                            } else {
                                0
                            }),
                        byte_start: bs,
                        byte_end: be,
                        code_block_id: start.node_index,
                    })
                } else {
                    // Spans within generated code won't mean anything to the user, suppress
                    // them.
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if span.is_none() {
            let expansion_span_json = &span_json["expansion"]["span"];
            if !expansion_span_json.is_empty() {
                let mut message = SpannedMessage::from_json(expansion_span_json, code_block);
                if message.span.is_some() {
                    if let Some(label) = span_json["label"].as_str() {
                        message.label = label.to_owned();
                    }
                    message.is_primary |= span_json["is_primary"].as_bool().unwrap_or(false);
                    return message;
                }
            }
        }
        SpannedMessage {
            span,
            lines: Vec::new(),
            label: span_json["label"]
                .as_str()
                .map(|s| s.to_owned())
                .unwrap_or_else(String::new),
            is_primary: span_json["is_primary"].as_bool().unwrap_or(false),
        }
    }

    pub(crate) fn from_segment_span(segment: &Segment, span: Span) -> SpannedMessage {
        SpannedMessage {
            span: Some(span),
            lines: segment.code.lines().map(|line| line.to_owned()).collect(),
            label: String::new(),
            is_primary: true,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    CompilationErrors(Vec<CompilationError>),
    TypeRedefinedVariablesLost(Vec<String>),
    Message(String),
    SubprocessTerminated(String),
}

impl std::error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::CompilationErrors(errors) => {
                for error in errors {
                    write!(f, "{}", error.message())?;
                }
            }
            Error::TypeRedefinedVariablesLost(variables) => {
                write!(
                    f,
                    "A type redefinition resulted in the following variables being lost: {}",
                    variables.join(", ")
                )?;
            }
            Error::Message(message) | Error::SubprocessTerminated(message) => {
                write!(f, "{}", message)?
            }
        }
        Ok(())
    }
}

impl From<std::fmt::Error> for Error {
    fn from(error: std::fmt::Error) -> Self {
        Error::Message(error.to_string())
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Message(error.to_string())
    }
}

impl From<json::Error> for Error {
    fn from(error: json::Error) -> Self {
        Error::Message(error.to_string())
    }
}

impl<'a> From<&'a io::Error> for Error {
    fn from(error: &'a io::Error) -> Self {
        Error::Message(error.to_string())
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(error: std::str::Utf8Error) -> Self {
        Error::Message(error.to_string())
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Error::Message(message)
    }
}

impl<'a> From<&'a str> for Error {
    fn from(message: &str) -> Self {
        Error::Message(message.to_owned())
    }
}

impl From<anyhow::Error> for Error {
    fn from(error: anyhow::Error) -> Self {
        Error::Message(error.to_string())
    }
}

impl From<libloading::Error> for Error {
    fn from(error: libloading::Error) -> Self {
        Error::Message(error.to_string())
    }
}

macro_rules! _err {
    ($e:expr) => {$crate::Error::from($e)};
    ($fmt:expr, $($arg:tt)+) => {$crate::errors::Error::from(format!($fmt, $($arg)+))}
}
pub(crate) use _err as err;

macro_rules! _bail {
    ($($arg:tt)+) => {return Err($crate::errors::err!($($arg)+))}
}
pub(crate) use _bail as bail;
