//! 语言标签 → 提示词里可读的语言名。
//!
//! 只用标签本身查表，**不做变体合并**：`zh-Hans` 与 `zh-Hant`、`pt` 与 `pt-BR` 是不同目标，
//! 合并会让模型按错误的书写习惯产出。表里没有的标签直接回显标签，不猜语言。

/// 取标签对应的中文语言名；未知标签回显原标签。
pub fn display_name(tag: &str) -> &str {
    match tag {
        "zh-Hans" => "简体中文",
        "zh-Hant" => "繁体中文",
        "en" => "英语",
        "ko" => "韩语",
        "ja" => "日语",
        "es" => "西班牙语",
        "fr" => "法语",
        "ru" => "俄语",
        "th" => "泰语",
        "vi" => "越南语",
        "de" => "德语",
        "id" => "印尼语",
        "pt" => "葡萄牙语",
        "pt-BR" => "巴西葡萄牙语",
        "tr" => "土耳其语",
        "it" => "意大利语",
        _ => tag,
    }
}

/// 提示词里的语言描述，同时给出语言名与标签，避免同名语言互相混淆。
pub fn describe(tag: &str) -> String {
    let name = display_name(tag);
    if name == tag {
        tag.to_owned()
    } else {
        format!("{name}（{tag}）")
    }
}
