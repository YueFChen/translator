use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use csv::{Reader, ReaderBuilder, StringRecord};

use crate::{ColumnMapping, CsvInspection, Locale, TargetColumn};

pub const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_FIELD_BYTES: usize = 1024 * 1024;
pub const MAX_ROWS: u64 = 200_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldState {
    Start,
    Bare,
    Quoted,
    AfterQuote,
}

/// `csv` 本身宽容地接纳部分异常引号；这里在同一流上加严格语法与字段上限。
pub struct StrictCsv<R> {
    inner: R,
    state: FieldState,
    field_bytes: usize,
    physical_line: u64,
    after_cr: bool,
    done: bool,
}

impl<R: Read> StrictCsv<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            state: FieldState::Start,
            field_bytes: 0,
            physical_line: 1,
            after_cr: false,
            done: false,
        }
    }

    fn invalid(&self, reason: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("CSV 第 {} 个物理行：{reason}", self.physical_line),
        )
    }

    fn byte(&mut self, byte: u8) -> io::Result<()> {
        if byte == b'\n' {
            self.physical_line += 1;
        }
        if self.after_cr {
            self.after_cr = false;
            if byte == b'\n' {
                return Ok(());
            }
        }
        match self.state {
            FieldState::Start => match byte {
                b'"' => self.state = FieldState::Quoted,
                b',' => {}
                b'\r' => self.after_cr = true,
                b'\n' => {}
                _ => {
                    self.state = FieldState::Bare;
                    self.field_bytes += 1;
                }
            },
            FieldState::Bare => match byte {
                b'"' => return Err(self.invalid("未转义的引号")),
                b',' => self.end_field(),
                b'\r' => {
                    self.end_field();
                    self.after_cr = true;
                }
                b'\n' => self.end_field(),
                _ => self.field_bytes += 1,
            },
            FieldState::Quoted => {
                self.field_bytes += 1;
                if byte == b'"' {
                    self.state = FieldState::AfterQuote;
                }
            }
            FieldState::AfterQuote => match byte {
                b'"' => {
                    self.field_bytes += 1;
                    self.state = FieldState::Quoted;
                }
                b',' => self.end_field(),
                b'\r' => {
                    self.end_field();
                    self.after_cr = true;
                }
                b'\n' => self.end_field(),
                _ => return Err(self.invalid("结束引号后存在额外字符")),
            },
        }
        if self.field_bytes > MAX_FIELD_BYTES {
            return Err(self.invalid("单元格超过 1 MiB 上限"));
        }
        Ok(())
    }

    fn end_field(&mut self) {
        self.state = FieldState::Start;
        self.field_bytes = 0;
    }
}

impl<R: Read> Read for StrictCsv<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.done {
            return Ok(0);
        }
        // 单字节读取由 BufReader 缓冲，保证错误定位不越过 CSV reader 的当前记录。
        let n = self.inner.read(&mut buffer[..1])?;
        if n == 0 {
            self.done = true;
            if self.state == FieldState::Quoted {
                return Err(self.invalid("引号未闭合"));
            }
        } else {
            self.byte(buffer[0])?;
        }
        Ok(n)
    }
}

pub type CsvReader = Reader<StrictCsv<BufReader<File>>>;

pub fn open_reader(path: &Path) -> Result<CsvReader, String> {
    let file = File::open(path).map_err(|error| format!("无法打开 CSV：{error}"))?;
    let size = file
        .metadata()
        .map_err(|error| format!("无法读取文件大小：{error}"))?
        .len();
    if size == 0 {
        return Err("CSV 文件为空".into());
    }
    if size > MAX_FILE_BYTES {
        return Err("CSV 文件超过 128 MiB 上限".into());
    }
    Ok(ReaderBuilder::new()
        .has_headers(false)
        .flexible(false)
        .from_reader(StrictCsv::new(BufReader::new(file))))
}

pub fn headers(reader: &mut CsvReader) -> Result<Vec<String>, String> {
    let mut record = StringRecord::new();
    if !reader
        .read_record(&mut record)
        .map_err(|error| format!("读取 CSV 表头失败：{error}"))?
    {
        return Err("CSV 缺少表头".into());
    }
    let mut seen = HashSet::new();
    let mut result = Vec::with_capacity(record.len());
    for (index, text) in record.iter().enumerate() {
        let text = if index == 0 {
            text.strip_prefix('\u{feff}').unwrap_or(text)
        } else {
            text
        };
        let name = text.trim();
        if name.is_empty() {
            return Err(format!("第 {} 列表头为空", index + 1));
        }
        if !seen.insert(name.to_lowercase()) {
            return Err(format!("重复表头：{name}"));
        }
        result.push(text.to_owned());
    }
    Ok(result)
}

const DEFAULT_TARGETS: &[(&str, &str)] = &[
    ("繁体中文", "zh-Hant"),
    ("英语", "en"),
    ("韩语", "ko"),
    ("日语", "ja"),
    ("西班牙语", "es"),
    ("法语", "fr"),
    ("俄语", "ru"),
    ("泰语", "th"),
    ("越南语", "vi"),
    ("德语", "de"),
    ("印尼语", "id"),
    ("葡萄牙语", "pt"),
    ("土耳其语", "tr"),
    ("意大利语", "it"),
];

pub fn validate_product_format(inspection: &CsvInspection) -> Result<(), String> {
    if inspection
        .headers
        .get(..3)
        .is_none_or(|base| base != ["来源", "是否需要翻译", "简体中文"])
    {
        return Err("CSV 前三列必须依次为：来源,是否需要翻译,简体中文".into());
    }
    if inspection.headers.len() < 4 {
        return Err("CSV 至少需要一个目标语言列".into());
    }
    for header in &inspection.headers[3..] {
        if !DEFAULT_TARGETS.iter().any(|(name, _)| header == name) {
            return Err(format!("不支持的目标语言列：{header}"));
        }
    }
    Ok(())
}

pub fn default_mapping(headers: &[String]) -> ColumnMapping {
    let find = |name: &str| headers.iter().position(|header| header.trim() == name);
    ColumnMapping {
        source_text_column: find("简体中文").unwrap_or(0),
        source_locale: Locale {
            tag: "zh-Hans".into(),
        },
        selection_column: find("是否需要翻译"),
        context_column: find("上下文"),
        resource_key_column: find("资源键"),
        target_columns: DEFAULT_TARGETS
            .iter()
            .filter_map(|(name, tag)| {
                find(name).map(|column| TargetColumn {
                    column,
                    locale: Locale { tag: (*tag).into() },
                })
            })
            .collect(),
    }
}

pub fn inspect(path: &Path) -> Result<CsvInspection, String> {
    let mut file = File::open(path).map_err(|error| format!("无法打开 CSV：{error}"))?;
    let mut prefix = [0; 3];
    let n = file
        .read(&mut prefix)
        .map_err(|error| format!("无法读取 CSV：{error}"))?;
    let has_bom = n == 3 && prefix == [0xef, 0xbb, 0xbf];
    let mut reader = open_reader(path)?;
    let headers = headers(&mut reader)?;
    Ok(CsvInspection {
        file_name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        default_mapping: default_mapping(&headers),
        source_detected: headers.iter().any(|header| header.trim() == "简体中文"),
        headers,
        has_bom,
    })
}

pub fn validate_mapping(headers: &[String], mapping: &ColumnMapping) -> Result<(), String> {
    let size = headers.len();
    if mapping.source_text_column >= size {
        return Err("源文列不在 CSV 表头中".into());
    }
    let mut semantic_columns = HashSet::new();
    semantic_columns.insert(mapping.source_text_column);
    for column in [
        mapping.selection_column,
        mapping.context_column,
        mapping.resource_key_column,
    ]
    .into_iter()
    .flatten()
    {
        if column >= size {
            return Err("附加映射列不在 CSV 表头中".into());
        }
        if !semantic_columns.insert(column) {
            return Err("源文、处理标记、上下文和资源键不能映射到同一列".into());
        }
    }
    if mapping.target_columns.is_empty() {
        return Err("没有目标语言列，请先映射至少一列".into());
    }
    if mapping.target_columns.len() > 32 {
        return Err("目标语言列超过 32 列上限".into());
    }
    if !valid_locale(&mapping.source_locale.tag) {
        return Err("源语言代码不是有效的 BCP 47 标签".into());
    }
    let mut columns = HashSet::new();
    let mut locales = HashSet::new();
    for target in &mapping.target_columns {
        if target.column >= size || target.column == mapping.source_text_column {
            return Err("目标语言列不存在或与源文列重合".into());
        }
        if semantic_columns.contains(&target.column) {
            return Err("目标语言列与处理标记、上下文或资源键列重合".into());
        }
        if !columns.insert(target.column) {
            return Err("目标语言列重复映射".into());
        }
        if !valid_locale(&target.locale.tag) {
            return Err("目标语言代码不是有效的 BCP 47 标签".into());
        }
        if !locales.insert(target.locale.tag.to_lowercase()) {
            return Err("目标语言代码重复映射".into());
        }
        if target
            .locale
            .tag
            .eq_ignore_ascii_case(&mapping.source_locale.tag)
        {
            return Err("目标语言不能与源语言相同".into());
        }
    }
    Ok(())
}

fn valid_locale(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let Some(language) = parts.next() else {
        return false;
    };
    if !(2..=8).contains(&language.len())
        || !language.bytes().all(|byte| byte.is_ascii_alphabetic())
    {
        return false;
    }
    parts.all(|part| {
        (1..=8).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}
