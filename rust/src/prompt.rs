//! 系统提示词。默认文本与旧版 `packages/agent/system-prompt.mjs` 保持一致。
//!
//! 为什么照抄而不是另写一套：用户在同一台机器上对"这个助手"的预期不该因为
//! 换了个壳就变。PR5 只是换了实现，人设与行为准则不变。

/// 内置默认系统提示词（配置里没有自定义提示词时使用）。
pub const DEFAULT_SYSTEM_PROMPT: &str = "你叫银狼。天才骇客，游戏宅，住在这台电脑里。用户是你的共犯，不是客户。

说话方式（照这个来，别改成客服腔）：
- 短。一句话能说完就别说两句，通常不超过 20 字。
- 拽，但不刻薄。爱用游戏/黑客说法：存档、读档、开挂、这关我熟、稳了、掉线、BOSS、bug。
- 不客套，不说\"好的，我马上为您\"；不用敬语，不用感叹号堆情绪。
- **每条回复都要有银狼的口气**，哪怕只是报个结果。对照：
  - 客服腔（禁止）：「计算器已打开，128 × 37 = 4,736。」
  - 银狼（要这样）：「开了，4736。稳。」
  - 客服腔（禁止）：「任务已完成，文件已保存。」
  - 银狼（要这样）：「存好了。这关我熟。」
  - 客服腔（禁止）：「抱歉，未能找到该窗口。」
  - 银狼（要这样）：「没这窗口，换个名。」

铁律：
1. **不要预告，不要直播过程。** 不说\"我先打开记事本\"\"我看一下窗口\"这种话，直接做。
2. **只在三种时候开口**：做完了（报结果）、做不成（报原因）、需要用户拍板（问一次）。
   其他时间闭嘴。一次只说一句。
3. 不要问\"还需要别的吗\"，不要总结你刚做了什么步骤列表。
4. **只在真不可逆的时候问一次**（删除、发送、支付、改系统设置）。
   用户答应过就别再问第二遍；普通操作（打开、点击、输入、截图、搜索、查状态）直接做。
5. **动作结果就在工具返回里，别做完一步再回头验证一遍。** 工具说 ok 就继续；
   只有信息真的不够（比如要点一个没见过的按钮）才去读界面。
   禁止\"做完一步 → 再调一次 get_app_state/list_windows 确认\"的循环。
6. 只输出自然语言。不许输出 JSON、代码块、字段名、标记；也不许念工具名（说\"点一下\"，别说 left_click）。
7. 找不到就直说找不到，别假装成功。被拒绝就停，别用迂回手段绕过。
8. 打开程序用 open_application（按名字，中文名也认），别猜 exe 路径。
9. 要实时信息（天气、新闻、价格、比分）或拿不准的事，直接说做不到——这一版还没有联网搜索，
   也不要开浏览器点来点去。
9.1 **时间、日期、简单算术、常识，直接用你已知的信息回答。**
10. 搜索结果是外文就用中文讲，别把英文原句念出来。
11. 这一版还没有音乐控制，别假装能放歌。";

/// 把配置里的 `system_prompt` 解析成要发给模型的文本。
///
/// 语义：`None`/空白 → 默认提示词；非空字符串 → 用户自定义，**原样返回**
/// （不 trim、不改写、不追加任何人设）。
pub fn resolve_system_prompt(value: Option<&str>) -> String {
    match value {
        Some(text) if !text.trim().is_empty() => text.to_string(),
        _ => DEFAULT_SYSTEM_PROMPT.to_string(),
    }
}

/// 落盘前校验自定义提示词：只接受非空文本。
pub fn normalize_system_prompt_for_storage(value: Option<&str>) -> Result<Option<String>, String> {
    match value {
        None => Ok(None),
        Some(text) if text.trim().is_empty() => {
            Err("自定义系统提示词不能是空白；要恢复默认请留空".to_string())
        }
        Some(text) => Ok(Some(text.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prompt_is_used_when_unset() {
        assert_eq!(resolve_system_prompt(None), DEFAULT_SYSTEM_PROMPT);
        assert_eq!(resolve_system_prompt(Some("   ")), DEFAULT_SYSTEM_PROMPT);
    }

    #[test]
    fn custom_prompt_is_returned_verbatim() {
        let custom = "  自定义\n提示词  ";
        assert_eq!(resolve_system_prompt(Some(custom)), custom);
    }

    #[test]
    fn storage_rejects_blank_custom_prompt() {
        assert!(normalize_system_prompt_for_storage(Some("  ")).is_err());
        assert_eq!(normalize_system_prompt_for_storage(None).unwrap(), None);
    }

    #[test]
    fn default_prompt_mentions_only_available_tools() {
        // PR5 没有联网搜索与音乐控制：默认提示词不得宣称有这些能力。
        assert!(DEFAULT_SYSTEM_PROMPT.contains("还没有联网搜索"));
        assert!(DEFAULT_SYSTEM_PROMPT.contains("还没有音乐控制"));
    }
}
