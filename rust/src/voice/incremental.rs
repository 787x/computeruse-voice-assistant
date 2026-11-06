//! 有界流式 Markdown 口语投影；控制符、代码和 URL 不进入句子队列。
#[derive(Default)]
pub struct Sentences {
    pending: String,
    token: String,
    skip_token: bool,
    token_at_line_start: bool,
    ticks: usize,
    code: usize,
    link_url: usize,
    after_label: bool,
    tag: bool,
    line_start: bool,
}
impl Sentences {
    pub fn push(&mut self, text: &str) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for c in text.chars() {
            if c == '`' {
                self.ticks += 1;
                continue;
            }
            if self.ticks > 0 {
                let ticks = std::mem::take(&mut self.ticks);
                if self.code == 0 {
                    self.flush_token();
                    self.code = ticks;
                } else if ticks >= self.code {
                    self.code = 0;
                }
            }
            if self.code > 0 {
                if c == '\n' && self.code < 3 {
                    self.code = 0;
                } else {
                    continue;
                }
            }
            if self.tag {
                if c == '>' || c == '\n' {
                    self.tag = false;
                }
                continue;
            }
            if self.link_url > 0 {
                if c == '\n' {
                    self.link_url = 0;
                    self.pending.push('\n');
                    self.drain(&mut out, false);
                    continue;
                }
                match c {
                    '(' => self.link_url += 1,
                    ')' => self.link_url -= 1,
                    _ => {}
                }
                continue;
            }
            if self.after_label {
                self.after_label = false;
                if c == '(' {
                    self.flush_token();
                    self.link_url = 1;
                    continue;
                }
            }
            match c {
                '<' => {
                    self.flush_token();
                    self.tag = true;
                }
                '[' => {}
                ']' => self.after_label = true,
                '*' | '_' => {}
                '#' | '>'
                    if self.line_start || (self.pending.is_empty() && self.token.is_empty()) => {}
                '\n' => {
                    self.flush_token();
                    self.pending.push('\n');
                    self.line_start = true;
                }
                c if c.is_whitespace() => {
                    self.flush_token();
                    self.pending.push(c);
                }
                '。' | '？' | '！' => {
                    self.flush_token();
                    self.pending.push(c);
                    self.line_start = false;
                }
                _ => {
                    if self.token.is_empty() {
                        self.token_at_line_start = self.line_start || self.pending.is_empty();
                    }
                    self.token.push(c);
                    self.line_start = false;
                }
            }
            // 超长无空白词：有界处理，URL 整段跳过，其余以自然字符块降级。
            if self.token.len() >= 2048 {
                self.skip_token |= self.token.contains("://") || self.token.starts_with("www.");
                if self.skip_token {
                    self.token.clear();
                } else {
                    self.flush_token();
                }
            }
            self.drain(&mut out, false);
        }
        Ok(out)
    }
    fn flush_token(&mut self) {
        let token = std::mem::take(&mut self.token);
        if std::mem::take(&mut self.skip_token) {
            return;
        }
        if token.contains("://")
            || token.starts_with("www.")
            || (self.token_at_line_start && (token == "-" || token == "+"))
            || (self.token_at_line_start
                && !token.is_empty()
                && token.ends_with('.')
                && token[..token.len() - 1].chars().all(|c| c.is_ascii_digit()))
        {
            return;
        }
        self.pending.push_str(&token);
    }
    fn drain(&mut self, out: &mut Vec<String>, final_input: bool) {
        loop {
            let boundary = self.pending.char_indices().find_map(|(i, c)| {
                let end = i + c.len_utf8();
                if matches!(c, '。' | '？' | '！' | '\n') {
                    return Some(end);
                }
                if matches!(c, '.' | '?' | '!') {
                    let before = &self.pending[..i];
                    let token = before.split_whitespace().last().unwrap_or("");
                    let after = &self.pending[end..];
                    let abbreviation = ["dr", "mr", "mrs", "ms", "prof", "etc", "vs", "jr", "sr"]
                        .contains(&token.to_ascii_lowercase().as_str());
                    if after.starts_with(char::is_whitespace)
                        && after
                            .trim_start()
                            .chars()
                            .next()
                            .is_some_and(|n| n.is_ascii_uppercase())
                        && token.len() > 2
                        && !token.contains(['.', ':', '/'])
                        && !abbreviation
                    {
                        return Some(end);
                    }
                }
                // 无句号长段落的有限降级；已经排除代码与 URL，最多保留约 1K 字。
                if i >= 1024 && (c.is_whitespace() || matches!(c, '，' | ',' | '；')) || i >= 3072
                {
                    return Some(end);
                }
                None
            });
            let end = match boundary {
                Some(end) => end,
                None if final_input && !self.pending.trim().is_empty() => self.pending.len(),
                None => break,
            };
            let rest = self.pending.split_off(end);
            let sentence = std::mem::replace(&mut self.pending, rest);
            if !sentence.trim().is_empty() {
                out.push(sentence.trim().to_string());
            }
        }
    }
    pub fn finish(&mut self) -> Vec<String> {
        if self.code == 0 && self.ticks == 0 && !self.tag && self.link_url == 0 {
            self.flush_token();
        }
        let mut out = Vec::new();
        self.drain(&mut out, true);
        out
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn collect(parts: &[&str]) -> Vec<String> {
        let mut s = Sentences::default();
        let mut out = Vec::new();
        for p in parts {
            out.extend(s.push(p).unwrap());
        }
        out.extend(s.finish());
        out
    }
    #[test]
    fn markdown_arbitrary_chunks() {
        let text = "# **标题**\n- 第一段。\n[链接](https://example.com)后的普通句。`隐藏代码`末句。\n```rust\n不能朗读。\n```\n最后一句。";
        let expected = collect(&[text]);
        for i in text.char_indices().map(|(i, _)| i) {
            assert_eq!(collect(&[&text[..i], &text[i..]]), expected);
        }
        let joined = expected.join("");
        assert_eq!(joined, "标题第一段。链接后的普通句。末句。最后一句。");
    }
    #[test]
    fn long_markdown_and_plain_text_progress() {
        let mut s = Sentences::default();
        let mut out = s.push("**标题**\n").unwrap();
        for _ in 0..2000 {
            out.extend(s.push("普通中文句子。").unwrap());
        }
        assert_eq!(out.len(), 2001);
        assert!(s.pending.len() < 4096);
        let out = s.push(&"无标点".repeat(20_000)).unwrap();
        assert!(!out.is_empty());
        assert!(s.pending.len() < 4096 && s.token.len() < 2048);
    }
    #[test]
    fn urls_decimals_abbreviations_and_code() {
        let out=collect(&["Dr. Smith has 3.14 items. Next sentence! More text。https://example.com/x?q=1 随后正常。"]);
        assert_eq!(
            out.join(""),
            "Dr. Smith has 3.14 items.Next sentence!More text。随后正常。"
        );
        assert_eq!(
            collect(&["答案是 42. 下一句。"]),
            vec!["答案是 42. 下一句。"]
        );
        assert_eq!(
            collect(&["`未闭合代码。\n", "恢复普通句。"]),
            vec!["恢复普通句。"]
        );
    }
}
