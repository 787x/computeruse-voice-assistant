//! 保守的流式可见文本分句。复杂 Markdown 留待成功结束后统一清理。
#[derive(Default)]
pub struct Sentences {
    pending: String,
    deferred: bool,
}
impl Sentences {
    pub fn push(&mut self, text: &str) -> Result<Vec<String>, String> {
        self.pending.push_str(text);
        if self.pending.len() > 32_768 {
            return Err("无可靠句界，增量播报缓冲已达上限".into());
        }
        self.deferred |= text.contains(['`', '*', '[', '<', '#', '_']);
        if self.deferred {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        while let Some((i, c)) = self.pending.char_indices().find(|(i, c)| {
            if matches!(c, '。' | '？' | '！' | '\n') {
                return true;
            }
            if !matches!(c, '.' | '?' | '!') {
                return false;
            }
            let before = &self.pending[..*i];
            let after = &self.pending[*i + c.len_utf8()..];
            let token = before.split_whitespace().last().unwrap_or("");
            let next = after.trim_start().chars().next();
            let abbreviation = ["dr", "mr", "mrs", "ms", "prof", "etc", "vs", "jr", "sr"]
                .contains(&token.to_ascii_lowercase().as_str());
            after.starts_with(char::is_whitespace)
                && next.is_some_and(|n| n.is_ascii_uppercase())
                && token.len() > 2
                && !token.contains(['.', ':', '/'])
                && !abbreviation
        }) {
            let rest = self.pending.split_off(i + c.len_utf8());
            let sentence = std::mem::replace(&mut self.pending, rest);
            if !sentence.trim().is_empty() {
                out.push(sentence);
            }
        }
        Ok(out)
    }
    pub fn finish(&mut self) -> Vec<String> {
        super::tts::prepare_incremental(&std::mem::take(&mut self.pending)).sentences
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_across_deltas() {
        let mut s = Sentences::default();
        assert!(s.push("第一").unwrap().is_empty());
        assert_eq!(s.push("句。第二句").unwrap(), vec!["第一句。"]);
        assert_eq!(s.finish(), vec!["第二句"]);
    }
    #[test]
    fn decimals_urls_code_and_bound() {
        let mut s = Sentences::default();
        assert!(s.push("3.14 Dr. https://example.com").unwrap().is_empty());
        assert!(s.push(" `代码。` ").unwrap().is_empty());
        assert!(s.push(&"字".repeat(12_000)).is_err());
    }
}
