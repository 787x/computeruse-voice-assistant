pub const PRESETS: &[(&str, &[&str])] = &[
    (
        "基础情绪",
        &[
            "开心", "悲伤", "愤怒", "恐惧", "惊讶", "兴奋", "委屈", "平静", "冷漠",
        ],
    ),
    (
        "复合情绪",
        &[
            "怅然", "欣慰", "无奈", "愧疚", "释然", "嫉妒", "厌倦", "忐忑", "动情",
        ],
    ),
    (
        "整体语调",
        &[
            "温柔", "高冷", "活泼", "严肃", "慵懒", "俏皮", "深沉", "干练", "凌厉",
        ],
    ),
    (
        "音色定位",
        &[
            "磁性", "醇厚", "清亮", "空灵", "稚嫩", "苍老", "甜美", "沙哑", "醇雅",
        ],
    ),
    (
        "人设腔调",
        &["夹子音", "御姐音", "正太音", "大叔音", "台湾腔"],
    ),
    ("方言", &["东北话", "四川话", "河南话", "粤语"]),
    ("角色扮演", &["孙悟空", "林黛玉"]),
    ("特殊风格", &["唱歌"]),
];
pub fn singing(s: &str) -> bool {
    s == "唱歌" || s.eq_ignore_ascii_case("sing") || s.eq_ignore_ascii_case("singing")
}
pub fn validate(tags: &[String]) -> Result<(), String> {
    let invalid = tags.len() > 16 || tags.iter().enumerate().any(|(i, s)| {
        s.is_empty() || s.trim() != s || s.encode_utf16().count() > 64 || tags[..i].contains(s)
        || s.chars().any(|c| c.is_control() || (c.is_whitespace() && c != ' ') || "()（）[]【】".contains(c)
            || matches!(c, '\u{00ad}' | '\u{0600}'..='\u{0605}' | '\u{061c}' | '\u{0890}'..='\u{0891}' | '\u{08e2}' | '\u{06dd}' | '\u{070f}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}' | '\u{13430}'..='\u{13455}' | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}' | '\u{e0001}' | '\u{e0020}'..='\u{e007f}'))
    }) || (tags.len() > 1 && tags.iter().any(|s| singing(s)));
    if invalid {
        Err(
            "整体风格无效：最多16项，每项1–64字符，不得重复或包含控制字符、括号；唱歌须单独使用"
                .into(),
        )
    } else {
        Ok(())
    }
}
pub fn prefix(tags: &[String]) -> String {
    if tags.is_empty() {
        String::new()
    } else {
        format!("({})", tags.join(" "))
    }
}

/// 只有添加操作；重复项不改变已有选择。先校验，再执行唱歌模式互斥。
pub fn add(tags: &mut Vec<String>, name: &str) -> Result<bool, String> {
    validate(&[name.to_string()])?;
    if tags.iter().any(|s| s == name) {
        return Ok(false);
    }
    let mut next = tags.clone();
    if singing(name) || next.iter().any(|s| singing(s)) {
        next.clear();
    }
    next.push(name.to_string());
    validate(&next)?;
    *tags = next;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn custom_add_is_idempotent_and_failed_add_is_atomic() {
        let mut tags = vec!["温柔".into()];
        assert_eq!(add(&mut tags, "温柔"), Ok(false));
        assert_eq!(tags, ["温柔"]);
        assert!(add(&mut tags, "坏\n标签").is_err());
        assert_eq!(tags, ["温柔"]);
        assert_eq!(add(&mut tags, "清亮"), Ok(true));
        assert_eq!(prefix(&tags), "(温柔 清亮)");
        assert_eq!(add(&mut tags, "singing"), Ok(true));
        assert_eq!(tags, ["singing"]);
        assert_eq!(add(&mut tags, "温柔"), Ok(true));
        assert_eq!(tags, ["温柔"]);
        tags = (0..16).map(|i| format!("标签{i}")).collect();
        let old = tags.clone();
        assert!(add(&mut tags, "第17项").is_err());
        assert_eq!(tags, old);
        assert!(add(&mut tags, &"😀".repeat(33)).is_err());
        assert_eq!(tags, old);
    }

    #[test]
    fn preset_count_and_validation() {
        assert_eq!(PRESETS.len(), 8);
        assert_eq!(PRESETS.iter().map(|(_, s)| s.len()).sum::<usize>(), 48);
        for tags in [
            vec!["唱歌", "温柔"],
            vec!["singing", "温柔"],
            vec!["温柔", "温柔"],
            vec!["换\n行"],
            vec!["(标签)"],
            vec!["隐\u{202e}形"],
        ] {
            assert!(validate(&tags.into_iter().map(str::to_string).collect::<Vec<_>>()).is_err());
        }
        assert_eq!(prefix(&["温柔".into(), "清亮".into()]), "(温柔 清亮)");
    }
}

pub fn decode(value: &str) -> Vec<String> {
    serde_json::from_str(value).unwrap_or_default()
}
pub fn encode(tags: &[String]) -> String {
    serde_json::to_string(tags).expect("字符串列表可序列化")
}
