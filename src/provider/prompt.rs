//! 提示词构造。
//!
//! 注入顺序固定为「硬规则 → 术语约束 → 不可译词 → 少量既有译文示例 → 带序号的源文片段」，
//! 超长时**反向**截断：先丢示例，再丢不可译词，最后丢术语约束，源文永不截断。这样"塞不下"
//! 的代价落在参考材料上，而不是让模型拿到半句话去翻译。
//!
//! 每批都给片段编号，响应必须按编号回填：批量结果的对齐靠 id，不靠顺序或贪婪匹配。

use crate::Locale;
use crate::provider::language;
use crate::provider::{BatchItem, BatchRequest, MemoryExample, TermConstraint};

/// 一次批量请求的提示词与上下文材料。
pub struct PromptParams<'a> {
    pub source_locale: &'a Locale,
    pub target_locale: &'a Locale,
    pub items: &'a [BatchItem],
    pub context: Option<&'a str>,
    pub constraints: &'a [TermConstraint],
    pub examples: &'a [MemoryExample],
    pub max_input_chars: usize,
}

/// 把批请求映射成提示词参数。适配器与长度判定共用同一份，免得两处各拼一遍基准不一致。
pub fn params_of(request: &BatchRequest, max_input_chars: usize) -> PromptParams<'_> {
    params_for_items(request, &request.items, max_input_chars)
}

fn params_for_items<'a>(
    request: &'a BatchRequest,
    items: &'a [BatchItem],
    max_input_chars: usize,
) -> PromptParams<'a> {
    PromptParams {
        source_locale: &request.source_locale,
        target_locale: &request.target_locale,
        items,
        context: request.context.as_deref(),
        constraints: &request.constraints,
        examples: &request.examples,
        max_input_chars,
    }
}

/// 在"整条提示词（system + user）不超过 `max_chars`"的前提下，最多能装下前几个片段。
///
/// 条目数与源文字符数都只是**粗筛**：一条超长源文就能顶满提示词。这里用真实渲染结果定稿，
/// 因为"装不下"只能靠丢弃尾部片段来解决——截断源文等于把最小翻译单元切成两半。
///
/// 收缩只会发生在尾部，且**至少保留第一个片段**：它再长也完整送出去，宁可与参考材料一起
/// 超一点上限，也不把一格原文拆开。返回 1 表示"只能装这一条"。
pub fn fit_items(request: &BatchRequest, max_chars: usize) -> usize {
    let mut count = request.items.len();
    while count > 1 {
        let params = params_for_items(request, &request.items[..count], max_chars);
        if prompt_chars(params) <= max_chars {
            break;
        }
        count -= 1;
    }
    count
}

/// 只量长度、不留内容：渲染出来的 `(system, user)` 一共多少字符。
///
/// 复用 [`build_prompt`]，因此量的是"真正会发出去的那条提示词"——参考材料该截已经截过。
fn prompt_chars(params: PromptParams<'_>) -> usize {
    let (system, user) = build_prompt(params);
    system.chars().count() + user.chars().count()
}

/// 构造 `(system, user)` 两条消息。
pub fn build_prompt(params: PromptParams<'_>) -> (String, String) {
    let mut system = String::from(
        "你是专业的游戏与应用本地化译者。译文要自然、简洁，符合目标语言的界面用语习惯。",
    );
    // 占位符是这一行最容易被模型"顺手改掉"的东西：变量名里常带中文（如 {1:s.昵称}），
    // 模型很容易把中文部分一起译掉；富文本属性的十六进制与大小写也常被改写。
    system.push_str(
        "源文中的 {变量}、<标签> 与字面 \\n 片段是占位符：必须逐字原样保留，\
         不得翻译、增删、改写、换行拆分或调整顺序；",
    );
    system.push_str(
        "花括号和尖括号内部的内容（含其中的中文、数字、颜色值）是标识符，一律不译、不改大小写；",
    );
    system.push_str("数字、版本号、路径、URL、邮箱一律照抄，不做本地化改写；");
    system.push_str(
        "遵守给定的术语译名与不可译词；不得添加源文没有的解释、括注或语气词，\
         也不得漏译、增译或合并片段的信息；",
    );
    system.push_str("若某个片段本身只有占位符、标点或空白而无可译文本，就原样返回该片段；");
    system.push_str(&format!(
        "只输出一个 JSON 对象，形如 {{\"translations\":[{{\"id\":1,\"translation\":\"译文\"}}],\"target\":\"{}\"}}：",
        params.target_locale.tag
    ));
    system.push_str(
        "translations 的元素必须与给定序号一一对应，条数不得增减，id 必须原样回填，不得重排或合并；",
    );
    // 译文要嵌在这个 JSON 的字符串里：引号与换行不转义就会让整批响应解析失败。
    system.push_str("译文里的双引号写成 \\\"、换行写成 \\n（源文里字面的 \\n 片段仍写作 \\\\n）；");
    system.push_str(
        "target 必须原样回显目标语言标签；除这一个 JSON 对象外不要输出任何文字、注释或代码块标记。",
    );

    let mut examples = params.examples;
    let mut constraints = params.constraints;
    let mut user = render(&params, constraints, examples);

    // 截断按优先级反向执行：示例最先丢，其次不可译词，最后术语约束；源文片段与指令永远保留。
    while user.chars().count() > params.max_input_chars && !examples.is_empty() {
        examples = &examples[..examples.len() - 1];
        user = render(&params, constraints, examples);
    }
    while user.chars().count() > params.max_input_chars
        && constraints.iter().any(|item| item.target_text.is_none())
    {
        constraints = drop_last_keep_term(constraints);
        user = render(&params, constraints, examples);
    }
    while user.chars().count() > params.max_input_chars && !constraints.is_empty() {
        constraints = &constraints[..constraints.len() - 1];
        user = render(&params, constraints, examples);
    }

    (system, user)
}

/// 去掉最后一条"保持原文不译"的约束。译名约束比它先保留：译名错是硬错，写法不统一只是瑕疵。
fn drop_last_keep_term(constraints: &[TermConstraint]) -> &[TermConstraint] {
    let keep = constraints
        .iter()
        .rposition(|item| item.target_text.is_none());
    match keep {
        Some(position) => &constraints[..position],
        None => constraints,
    }
}

fn render(
    params: &PromptParams<'_>,
    constraints: &[TermConstraint],
    examples: &[MemoryExample],
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "源语言：{}\n目标语言：{}\n",
        language::describe(&params.source_locale.tag),
        language::describe(&params.target_locale.tag),
    ));
    if let Some(context) = params.context.filter(|value| !value.is_empty()) {
        out.push_str(&format!("场景上下文：{context}\n"));
    }
    let naming: Vec<&TermConstraint> = constraints
        .iter()
        .filter(|item| item.target_text.is_some())
        .collect();
    if !naming.is_empty() {
        out.push_str("必须使用的术语译名：\n");
        for item in naming {
            out.push_str(&format!(
                "- {} → {}\n",
                item.source_surface,
                item.target_text.as_deref().unwrap_or_default()
            ));
        }
    }
    let keep: Vec<&TermConstraint> = constraints
        .iter()
        .filter(|item| item.target_text.is_none())
        .collect();
    if !keep.is_empty() {
        out.push_str("以下词语保持原文、不翻译（句中出现时也照抄）：\n");
        for item in keep {
            out.push_str(&format!("- {}\n", item.source_surface));
        }
    }
    if !examples.is_empty() {
        out.push_str("同项目的既有译文，仅供参考风格，不要照抄到不同源文上：\n");
        for item in examples {
            out.push_str(&format!(
                "- {} → {}\n",
                item.source_text.trim(),
                item.target_text.trim()
            ));
        }
    }
    out.push_str(&format!("请逐条翻译下列 {} 个片段：\n", params.items.len()));
    for item in params.items {
        out.push_str(&format!("{}. {}\n", item.id, item.source_text));
    }
    out
}

/// 从模型响应里取出译文并核对回显的语言标签，返回 `(id, 译文)`。
///
/// 只认结构化的 JSON 对象；解析不出就报 `Invalid`，由调度器把整批记为失败，
/// **不**用贪婪正则去"抢救"，那会把模型多余的说明文字混进译文。`target` 与请求不一致
/// 说明模型串了语言，这类结果为 [`crate::provider::ProviderError::Invalid`]，同样不落成功。
pub fn parse_translations(
    body: &[u8],
    expected_target: &str,
) -> Result<Vec<(u32, String)>, String> {
    #[derive(serde::Deserialize)]
    struct Choice {
        message: Message,
    }
    #[derive(serde::Deserialize)]
    struct Message {
        content: String,
    }
    #[derive(serde::Deserialize)]
    struct Envelope {
        choices: Vec<Choice>,
    }
    #[derive(serde::Deserialize)]
    struct Payload {
        #[serde(default)]
        translations: Vec<Entry>,
        #[serde(default)]
        target: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        id: u32,
        translation: String,
    }

    let envelope: Envelope =
        serde_json::from_slice(body).map_err(|error| format!("响应不是合法 JSON：{error}"))?;
    let content = envelope
        .choices
        .first()
        .ok_or("响应没有 choices")?
        .message
        .content
        .trim();
    let payload: Payload = serde_json::from_str(content)
        .map_err(|error| format!("模型输出不是 JSON 对象：{error}"))?;
    let target = payload
        .target
        .filter(|value| !value.is_empty())
        .ok_or("模型输出缺少 target 字段")?;
    if !target.eq_ignore_ascii_case(expected_target) {
        return Err(format!("模型返回的目标语言是 {target}"));
    }
    let mut items: Vec<(u32, String)> = Vec::with_capacity(payload.translations.len());
    for entry in payload.translations {
        let text = entry.translation.trim();
        if text.is_empty() {
            return Err(format!("片段 {} 的译文为空", entry.id));
        }
        items.push((entry.id, text.to_owned()));
    }
    if items.is_empty() {
        return Err("模型输出没有 translations 条目".into());
    }
    Ok(items)
}

/// 从响应信封里读 token 用量；缺字段返回 `(None, None)`，不按字数估算。
pub fn parse_usage(body: &[u8]) -> (Option<u64>, Option<u64>) {
    #[derive(serde::Deserialize)]
    struct Usage {
        #[serde(default)]
        prompt_tokens: Option<u64>,
        #[serde(default)]
        completion_tokens: Option<u64>,
    }
    #[derive(serde::Deserialize)]
    struct Envelope {
        #[serde(default)]
        usage: Option<Usage>,
    }
    match serde_json::from_slice::<Envelope>(body) {
        Ok(envelope) => envelope.usage.map_or((None, None), |usage| {
            (usage.prompt_tokens, usage.completion_tokens)
        }),
        Err(_) => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u32, text: &str) -> BatchItem {
        BatchItem {
            id,
            source_text: text.to_owned(),
        }
    }

    fn constraint(source: &str, target: Option<&str>) -> TermConstraint {
        TermConstraint {
            source_surface: source.to_owned(),
            target_text: target.map(str::to_owned),
        }
    }

    fn locales() -> (Locale, Locale) {
        (
            Locale {
                tag: "zh-Hans".into(),
            },
            Locale { tag: "en".into() },
        )
    }

    fn params<'a>(
        locales: &'a (Locale, Locale),
        items: &'a [BatchItem],
        constraints: &'a [TermConstraint],
        examples: &'a [MemoryExample],
        max: usize,
    ) -> PromptParams<'a> {
        PromptParams {
            source_locale: &locales.0,
            target_locale: &locales.1,
            items,
            context: None,
            constraints,
            examples,
            max_input_chars: max,
        }
    }

    /// 硬规则必须点名几件在本地化里最容易被模型"顺手改掉"的东西：变量内部不译、
    /// 形似正文的标识符照抄，且唯一输出是一个可解析的 JSON 对象。
    #[test]
    fn system_message_states_the_hard_rules() {
        let locales = locales();
        let items = [item(1, "你好")];
        let (system, _) = build_prompt(params(&locales, &items, &[], &[], 8_000));
        assert!(system.contains("花括号和尖括号"), "{system}");
        assert!(system.contains("不可译词"), "{system}");
        assert!(system.contains("translations"), "{system}");
        // 译文里的引号与换行必须转义，否则整批响应会解析失败。
        assert!(system.contains("双引号写成"), "{system}");
    }

    /// 词条按"译名"与"保持原文"分成两组渲染，不可译词不能混进译名列表里。
    #[test]
    fn constraints_render_naming_and_keep_terms_separately() {
        let locales = locales();
        let items = [item(1, "欢迎来到 Wonderland")];
        let constraints = [
            constraint("欢迎", Some("Welcome")),
            constraint("Wonderland", None),
        ];
        let (_, user) = build_prompt(params(&locales, &items, &constraints, &[], 8_000));
        assert!(
            user.contains("必须使用的术语译名：\n- 欢迎 → Welcome"),
            "{user}"
        );
        assert!(user.contains("保持原文、不翻译"), "{user}");
        assert!(user.contains("\n- Wonderland\n"), "{user}");
        assert!(user.contains("1. 欢迎来到 Wonderland"), "{user}");
    }

    /// 截断顺序：示例最先丢，其次不可译词，最后译名约束；源文片段与指令一条都不能少。
    ///
    /// 预算按上一轮的实际长度逐级收紧（而不是写死数字）：这样断言不会因为改一句措辞就失效，
    /// 同时逐级验证了"先牺牲谁"。
    #[test]
    fn truncation_drops_reference_material_before_source_text() {
        let locales = locales();
        let items = [item(1, "甲乙丙丁戊己庚辛壬癸")];
        let constraints = [
            constraint("甲", Some("A")),
            constraint("乙", Some("B")),
            constraint("Wonderland", None),
        ];
        let examples = [MemoryExample {
            source_text: "示例源文".into(),
            target_text: "example".into(),
        }];
        let build =
            |max: usize| build_prompt(params(&locales, &items, &constraints, &examples, max)).1;

        let full = build(4_000);
        assert!(full.contains("仅供参考风格"), "{full}");
        // 少一个字符的预算：只够牺牲最不重要的既有译文示例，不可译词与译名都还在。
        let tight = build(full.chars().count() - 1);
        assert!(!tight.contains("仅供参考风格"), "{tight}");
        assert!(tight.contains("以下词语保持原文"), "{tight}");
        assert!(tight.contains("甲 → A"), "{tight}");
        // 再收紧一级：轮到不可译词让步，译名约束仍然保留。
        let tighter = build(tight.chars().count() - 1);
        assert!(!tighter.contains("Wonderland"), "{tighter}");
        assert!(tighter.contains("甲 → A"), "{tighter}");
        // 最后才轮到译名约束，且源文片段自始至终完整。
        let tightest = build(tighter.chars().count() - 1);
        assert!(!tightest.contains("乙 → B"), "{tightest}");
        for text in [&full, &tight, &tighter, &tightest] {
            assert!(text.contains("甲乙丙丁戊己庚辛壬癸"), "{text}");
            assert!(text.contains("1. "), "{text}");
        }
    }

    /// 装不下只能"少装几条"：一条超长源文能让批次收缩到只剩它自己，但**那条源文必须完整**。
    ///
    /// 这是"字符数 + 行数综合判定"里最要紧的一半：收缩的粒度是"整条片段"，不是字符——
    /// 把源文裁一半交给模型，等于让它照着半句话翻译，比不翻还糟。
    #[test]
    fn fit_items_keeps_the_whole_unit_even_when_it_alone_is_too_long() {
        let locales = locales();
        let long = "长".repeat(400);
        let items = [item(1, &long), item(2, "短句"), item(3, "另一句")];
        let request = BatchRequest {
            source_locale: locales.0.clone(),
            target_locale: locales.1.clone(),
            context: None,
            items: items.to_vec(),
            constraints: Vec::new(),
            examples: Vec::new(),
        };
        // 预算再小也返回 1 而不是 0：完整送出一条，好过一条都不发。
        assert_eq!(fit_items(&request, 1), 1);
        // 用真实渲染长度定稿：够两条就是两条，少一个字符就退到一条。
        let two = prompt_chars(params_for_items(&request, &request.items[..2], 8_000));
        assert_eq!(fit_items(&request, two), 2);
        assert_eq!(fit_items(&request, two - 1), 1);
        // 收缩发生在尾部，被保留的那条源文一字不少。
        let (_, user) = build_prompt(params_for_items(&request, &request.items[..1], 8_000));
        assert!(user.contains(&long), "{user}");
        assert!(!user.contains("短句"), "{user}");
    }

    /// 只认结构化 JSON：多出来的说明文字、缺 target、错语言都不算成功。
    #[test]
    fn parser_rejects_anything_but_the_contract() {
        let ok = r#"{"choices":[{"message":{"content":"{\"translations\":[{\"id\":1,\"translation\":\"Hello\"}],\"target\":\"en\"}"}}]}"#;
        assert_eq!(
            parse_translations(ok.as_bytes(), "en").unwrap(),
            vec![(1, "Hello".to_owned())]
        );
        let wrong_language = r#"{"choices":[{"message":{"content":"{\"translations\":[{\"id\":1,\"translation\":\"Hallo\"}],\"target\":\"de\"}"}}]}"#;
        assert!(parse_translations(wrong_language.as_bytes(), "en").is_err());
        // 模型在 JSON 前加了客套话：结构就不再是"一个 JSON 对象"，必须判失败而不是抠出来。
        let prose = r#"{"choices":[{"message":{"content":"好的，以下是译文：{\"translations\":[{\"id\":1,\"translation\":\"Hi\"}],\"target\":\"en\"}"}}]}"#;
        assert!(parse_translations(prose.as_bytes(), "en").is_err());
        // 缺条目由调度器按 id 判失败，解析本身仍是成功的。
        let partial = r#"{"choices":[{"message":{"content":"{\"translations\":[{\"id\":2,\"translation\":\"Two\"}],\"target\":\"en\"}"}}]}"#;
        assert_eq!(
            parse_translations(partial.as_bytes(), "en").unwrap(),
            vec![(2, "Two".to_owned())]
        );
    }
}
