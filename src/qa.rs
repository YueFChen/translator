use std::collections::BTreeMap;

use crate::{IssueSeverity, Locale, PlaceholderSignature, QualityIssue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatInfo {
    variables: BTreeMap<String, u32>,
    tags: Vec<String>,
    literal_newlines: u32,
    real_newlines: u32,
    carriage_returns: u32,
    has_format: bool,
}

impl FormatInfo {
    pub fn signature(&self) -> PlaceholderSignature {
        let canonical = serde_json::to_string(&(
            &self.variables,
            &self.tags,
            self.literal_newlines,
            self.real_newlines,
            self.carriage_returns,
        ))
        .expect("serializable format signature");
        PlaceholderSignature {
            schema_version: 1,
            canonical,
        }
    }

    pub fn has_format(&self) -> bool {
        self.has_format
    }

    pub fn has_real_newline(&self) -> bool {
        self.real_newlines > 0
    }

    pub fn has_carriage_return(&self) -> bool {
        self.carriage_returns > 0
    }
}

/// 解析变量身份和次数、富文本标签的配对与嵌套，以及真实/字面换行。
pub fn parse(text: &str) -> Result<FormatInfo, String> {
    let bytes = text.as_bytes();
    let mut variables = BTreeMap::new();
    let mut tags = Vec::new();
    let paired_tags = paired_rich_tags(text);
    let mut literal_newlines = 0;
    let mut real_newlines = 0;
    let mut carriage_returns = 0;
    let mut has_format = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => {
                if bytes[i + 1] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i + 1] == b'n' {
                    literal_newlines += 1;
                    has_format = true;
                    i += 2;
                    continue;
                }
            }
            b'{' => {
                if bytes.get(i + 1) == Some(&b'{') {
                    i += 2;
                    continue;
                }
                let Some(end) = text[i + 1..].find('}') else {
                    return Err("变量缺少结束大括号".into());
                };
                let end = i + 1 + end;
                let value = &text[i + 1..end];
                if value.is_empty() || value.contains('{') || value.contains('\n') {
                    return Err("变量内容无效".into());
                }
                *variables.entry(value.to_owned()).or_insert(0) += 1;
                has_format = true;
                i = end + 1;
                continue;
            }
            b'}' => {
                if bytes.get(i + 1) == Some(&b'}') {
                    i += 2;
                    continue;
                }
                return Err("存在未配对的结束大括号".into());
            }
            b'<' => {
                if let Some((end, body)) = paired_tags.get(&i) {
                    tags.push((*body).to_owned());
                    has_format = true;
                    i = *end;
                    continue;
                }
            }
            b'\n' => {
                real_newlines += 1;
            }
            b'\r' => {
                carriage_returns += 1;
            }
            _ => {}
        }
        i += 1;
    }
    Ok(FormatInfo {
        variables,
        tags,
        literal_newlines,
        real_newlines,
        carriage_returns,
        has_format,
    })
}

fn paired_rich_tags(text: &str) -> BTreeMap<usize, (usize, &str)> {
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find('<') {
        let start = cursor + offset;
        let Some(end_offset) = text[start + 1..].find('>') else {
            break;
        };
        let end = start + 1 + end_offset;
        let body = &text[start + 1..end];
        if let Some((name, closing)) = rich_tag(body) {
            tokens.push((start, end + 1, body, name, closing));
            cursor = end + 1;
        } else {
            cursor = start + 1;
        }
    }
    let mut paired = vec![false; tokens.len()];
    let mut stack = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if !token.4 {
            stack.push(index);
        } else if let Some(position) = stack.iter().rposition(|open| tokens[*open].3 == token.3) {
            paired[stack.remove(position)] = true;
            paired[index] = true;
        }
    }
    tokens
        .into_iter()
        .zip(paired)
        .filter_map(|((start, end, body, _, _), valid)| valid.then_some((start, (end, body))))
        .collect()
}

// 与 rich_editor/ui/src/markup.ts 的 PATTERNS 保持一致；其他尖括号内容是正文。
fn rich_tag(body: &str) -> Option<(&'static str, bool)> {
    match body {
        "b" => Some(("b", false)),
        "/b" => Some(("b", true)),
        "i" => Some(("i", false)),
        "/i" => Some(("i", true)),
        "/size" => Some(("size", true)),
        "/color" => Some(("color", true)),
        _ => {
            if let Some(size) = body.strip_prefix("size=")
                && !size.is_empty()
                && size.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Some(("size", false));
            }
            if let Some(color) = body.strip_prefix("color=") {
                let hex = color.strip_prefix('#').unwrap_or(color);
                if (hex.len() == 6 || hex.len() == 8)
                    && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Some(("color", false));
                }
            }
            None
        }
    }
}

pub fn issue(
    row_number: u64,
    target_locale: &Locale,
    code: &str,
    severity: IssueSeverity,
    details: impl Into<String>,
) -> QualityIssue {
    QualityIssue {
        row_number,
        target_locale: target_locale.clone(),
        code: code.into(),
        severity,
        details: details.into(),
    }
}

/// 判定一个单元格是否**形态上就是被切开的拼接片段**，返回命中的形态说明。
///
/// 上游按变量/标签把一句话切成多格导出时，单格拿不到完整结构；这类单元格单独送模型
/// 只会得到脱离语境的半句译文。这里只做**单元格自身**的形态判定，不推断相邻格属于
/// 哪一句——拼接顺序与槽位名不在 CSV 里，猜不得。
///
/// 命中即视为需要人工确认，不阻断导出：有些片段（如纯占位模板）本来就不该翻译。
pub fn fragment_reason(text: &str) -> Option<&'static str> {
    if text.trim_start().starts_with("</") {
        return Some("以没有对应开标签的结束标签开头");
    }
    if ends_with_open_tag(text) {
        return Some("以未闭合的开始标签结尾，标签的另一半在相邻格");
    }
    if starts_with_connector(text) {
        return Some("以连接标点开头，像是上一句的续写");
    }
    if stripped_is_empty(text) {
        return Some("去除变量与标签后没有可翻译的文本");
    }
    None
}

/// 文本 trim 后是否以仍未闭合的开标签结尾（只认富文本白名单标签，`a < b >` 不算）。
fn ends_with_open_tag(text: &str) -> bool {
    let trimmed = text.trim_end();
    let Some(start) = trimmed.rfind('<') else {
        return false;
    };
    let Some(body) = trimmed[start + 1..].strip_suffix('>') else {
        return false;
    };
    if body.contains('<') {
        return false;
    }
    matches!(rich_tag(body), Some((_, false)))
}

/// 以连接标点开头。只看中文全角标点与 `,;:`：句点等半角符号在 `.NET` 这类正文里
/// 是正常开头，误报比漏报更烦人。
fn starts_with_connector(text: &str) -> bool {
    matches!(
        text.trim_start().chars().next(),
        Some('，' | '。' | '；' | '、' | '：' | '！' | '？' | ',' | ';' | ':')
    )
}

/// 去掉变量、标签与空白后是否没有剩余文本。
///
/// 未闭合的 `{`、`<` 一律当普通正文，宁可漏报也不要把它当成"没有内容"。
fn stripped_is_empty(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let close = match chars[index] {
            ch if ch.is_whitespace() => {
                index += 1;
                continue;
            }
            '{' => '}',
            '<' => '>',
            _ => return false,
        };
        let Some(offset) = chars[index + 1..].iter().position(|ch| *ch == close) else {
            return false;
        };
        index += offset + 2;
    }
    true
}

pub fn compare(
    source: &FormatInfo,
    target: &FormatInfo,
    row: u64,
    locale: &Locale,
) -> Vec<QualityIssue> {
    let mut issues = Vec::new();
    if source.variables != target.variables {
        issues.push(issue(
            row,
            locale,
            "placeholder_mismatch",
            IssueSeverity::Blocking,
            "变量身份或出现次数与源文不一致",
        ));
    }
    if source.tags != target.tags {
        issues.push(issue(
            row,
            locale,
            "tag_mismatch",
            IssueSeverity::Blocking,
            "富文本标签、属性或嵌套顺序与源文不一致",
        ));
    }
    if source.literal_newlines != target.literal_newlines {
        issues.push(issue(
            row,
            locale,
            "literal_newline_mismatch",
            IssueSeverity::Blocking,
            "字面 \\n 片段数量与源文不一致",
        ));
    }
    if source.real_newlines != target.real_newlines {
        issues.push(issue(
            row,
            locale,
            "newline_mismatch",
            IssueSeverity::Review,
            "真实换行数量与源文不同",
        ));
    }
    if source.carriage_returns != target.carriage_returns {
        issues.push(issue(
            row,
            locale,
            "carriage_return_mismatch",
            IssueSeverity::Review,
            "回车数量与源文不同",
        ));
    }
    issues
}

pub fn numbers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() || (!current.is_empty() && (ch == '.' || ch == ',')) {
            current.push(ch);
        } else if !current.is_empty() {
            out.push(current.trim_end_matches([',', '.']).to_owned());
            current.clear();
        }
    }
    if !current.is_empty() {
        out.push(current.trim_end_matches([',', '.']).to_owned());
    }
    out
}
