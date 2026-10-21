// i18n module
use std::env;
use std::sync::LazyLock;

static CURRENT_LANG: LazyLock<&str> = LazyLock::new(|| {
    let from_config = crate::config::load_config().language;
    let lang = if !from_config.is_empty() {
        from_config
    } else {
        env::var("LANG").unwrap_or_default()
    };
    if lang.to_lowercase().starts_with("zh") {
        "zh-CN"
    } else {
        "en"
    }
});

static LOCALE_EN: LazyLock<std::collections::HashMap<&str, &str>> = LazyLock::new(|| {
    let mut m = std::collections::HashMap::new();

    // Status icons
    m.insert("sess.think", "◉ Working");
    m.insert("sess.reply", "● Reply");
    m.insert("sess.wait", "◌ Wait");
    m.insert("sess.unknown", "? Unknown");
    m.insert("sess.done", "✓ Done");

    // Column headers
    m.insert("col.ai", "AI");
    m.insert("col.session_name", "Session Name");
    m.insert("col.status", "Status");
    m.insert("col.model", "Model");
    m.insert("col.context", "Context");
    m.insert("col.ctx", "Ctx");
    m.insert("col.tokens", "Tokens");

    // Sessions detail
    m.insert("detail.session", "SESSION");
    m.insert("detail.children", "CHILDREN");
    m.insert("detail.subagents", "SUBAGENTS");
    m.insert("detail.mem", "MEM");
    m.insert("detail.ctx", "CTX");
    m.insert("detail.timeline", "TIMELINE");
    m.insert("detail.chat", "CHAT");
    m.insert("detail.file_audit", "FILE AUDIT");

    // Help panel
    m.insert("help.title", " Keybindings ");
    m.insert("help.navigation", "Navigation");
    m.insert("help.actions", "Actions");
    m.insert("help.views", "Views");
    m.insert("help.help", "Help");
    m.insert("help.press_key", " Press any key to close ");
    m.insert("help.select_session", "select session");
    m.insert("help.scroll_chat", "scroll chat history (End: latest)");
    m.insert("help.jump_tmux", "jump to session terminal");
    m.insert("help.filter", "filter sessions");
    m.insert("help.clear_filter", "clear filter / close overlay");
    m.insert("help.kill_session", "kill selected session");
    m.insert("help.refresh", "force refresh");
    m.insert("help.quit", "quit");
    m.insert("help.view_menu", "open view menu");
    m.insert("help.cycle_theme", "cycle theme / toggle tree");
    m.insert("help.toggle_timeline", "toggle timeline");
    m.insert("help.toggle_file_audit", "toggle file audit");
    m.insert(
        "help.toggle_chat_tools",
        "toggle tool calls/results in chat",
    );
    m.insert("help.toggle_panels", "toggle panels (quota/sessions)");
    m.insert(
        "help.mcp_suppress",
        "toggle mcp-server suppression in sessions panel",
    );
    m.insert("help.this_help", "this help");
    m.insert("help.legend", "Legend");
    m.insert(
        "help.launch_surface_suffix",
        "Claude: a = Desktop App, i = IDE extension (blank = CLI)",
    );

    // Footer
    m.insert("footer.select", "select");
    m.insert("footer.kill", "kill");
    m.insert("footer.filter", "filter");
    m.insert("footer.view", "view");
    m.insert("footer.help", "help");
    m.insert("footer.quit", "quit");
    m.insert("footer.sessions", "sessions");
    m.insert("footer.auto", "auto");
    m.insert("footer.esc_clear", "Esc clear, Enter keep");
    m.insert("footer.jump", "jump");

    // View menu
    m.insert("view.title", " View ");
    m.insert("view.on", "on");
    m.insert("view.off", "off");
    m.insert("view.action", "→");
    m.insert("view.tree_view", "tree view");
    m.insert("view.timeline", "timeline");
    m.insert("view.file_audit", "file audit");
    m.insert("view.chat_tools", "chat tool details");
    m.insert("view.quota_panel", "quota panel");
    m.insert("view.sessions_panel", "sessions panel");
    m.insert("view.mcp_session_hide", "mcp session hide");
    m.insert("view.cycle_theme", "cycle theme");
    m.insert("view.key_toggle", "key = toggle  ·  Esc = close ");

    // Quota panel
    m.insert("quota.5h", "5h");
    m.insert("quota.7d", "7d");
    m.insert("quota.no_data", "no data");
    m.insert("quota.abtop_setup", "agents-dashboard --setup");
    m.insert("quota.run_codex", "run codex once");
    m.insert("quota.total", "total");

    // Terminal size too small
    m.insert("term.too_small", "Terminal size too small:");
    m.insert("term.width", "Width");
    m.insert("term.height", "Height");
    m.insert("term.needed", "Needed for current config:");

    // Time formatting
    m.insert("time.m", "m");
    m.insert("time.h", "h");
    m.insert("time.d", "d");

    m
});

static LOCALE_ZH: LazyLock<std::collections::HashMap<&str, &str>> = LazyLock::new(|| {
    let mut m = std::collections::HashMap::new();

    // Status icons
    m.insert("sess.think", "◉ 思考");
    m.insert("sess.reply", "● Reply");
    m.insert("sess.wait", "◌ 等待");
    m.insert("sess.unknown", "? Unknown");
    m.insert("sess.done", "✓ 完成");

    // Column headers
    m.insert("col.ai", "AI");
    m.insert("col.session_name", "会话名称");
    m.insert("col.status", "状态");
    m.insert("col.model", "模型");
    m.insert("col.context", "上下文");
    m.insert("col.ctx", "上");
    m.insert("col.tokens", "Token");

    // Sessions detail
    m.insert("detail.session", "会话");
    m.insert("detail.children", "子进程");
    m.insert("detail.subagents", "子代理");
    m.insert("detail.mem", "内存");
    m.insert("detail.ctx", "上下文");
    m.insert("detail.timeline", "时间线");
    m.insert("detail.chat", "聊天");
    m.insert("detail.file_audit", "文件审计");

    // Help panel
    m.insert("help.title", " 快捷键 ");
    m.insert("help.navigation", "导航");
    m.insert("help.actions", "操作");
    m.insert("help.views", "视图");
    m.insert("help.help", "帮助");
    m.insert("help.press_key", " 按任意键关闭 ");
    m.insert("help.select_session", "选择会话");
    m.insert("help.scroll_chat", "滚动聊天记录（End：最新）");
    m.insert("help.jump_tmux", "jump to session terminal");
    m.insert("help.filter", "过滤会话");
    m.insert("help.clear_filter", "清除过滤 / 关闭覆盖");
    m.insert("help.kill_session", "终止选中的会话");
    m.insert("help.refresh", "强制刷新");
    m.insert("help.quit", "退出");
    m.insert("help.view_menu", "打开视图菜单");
    m.insert("help.cycle_theme", "切换主题 / 切换树视图");
    m.insert("help.toggle_timeline", "切换时间线");
    m.insert("help.toggle_file_audit", "切换文件审计");
    m.insert("help.toggle_chat_tools", "切换聊天中的工具调用/结果");
    m.insert("help.toggle_panels", "切换面板 (配额/会话)");
    m.insert("help.mcp_suppress", "切换会话面板中的 MCP 服务器隐藏");
    m.insert("help.this_help", "显示帮助");
    m.insert("help.legend", "图例");
    m.insert(
        "help.launch_surface_suffix",
        "Claude: a = 桌面应用, i = IDE 扩展（无后缀 = 命令行）",
    );

    // Footer
    m.insert("footer.select", "选择");
    m.insert("footer.kill", "终止");
    m.insert("footer.filter", "过滤");
    m.insert("footer.view", "视图");
    m.insert("footer.help", "帮助");
    m.insert("footer.quit", "退出");
    m.insert("footer.sessions", "会话");
    m.insert("footer.auto", "自动");
    m.insert("footer.esc_clear", "Esc 清除，Enter 保留");
    m.insert("footer.jump", "跳转");

    // View menu
    m.insert("view.title", " 视图 ");
    m.insert("view.on", "开");
    m.insert("view.off", "关");
    m.insert("view.action", "→");
    m.insert("view.tree_view", "树视图");
    m.insert("view.timeline", "时间线");
    m.insert("view.file_audit", "文件审计");
    m.insert("view.chat_tools", "聊天工具详情");
    m.insert("view.quota_panel", "配额面板");
    m.insert("view.sessions_panel", "会话面板");
    m.insert("view.mcp_session_hide", "隐藏 MCP 会话");
    m.insert("view.cycle_theme", "切换主题");
    m.insert("view.key_toggle", "按键切换  ·  Esc 关闭 ");

    // Quota panel
    m.insert("quota.5h", "5小时");
    m.insert("quota.7d", "7天");
    m.insert("quota.no_data", "无数据");
    m.insert("quota.abtop_setup", "agents-dashboard --setup");
    m.insert("quota.run_codex", "运行一次 codex");
    m.insert("quota.total", "总计");

    // Terminal size too small
    m.insert("term.too_small", "终端尺寸过小:");
    m.insert("term.width", "宽度");
    m.insert("term.height", "高度");
    m.insert("term.needed", "当前配置需要:");

    // Time formatting
    m.insert("time.m", "分");
    m.insert("time.h", "时");
    m.insert("time.d", "天");

    m
});

pub fn t(key: &str) -> String {
    if *CURRENT_LANG == "zh-CN" {
        LOCALE_ZH
            .get(key)
            .map(|s| s.to_string())
            .unwrap_or_else(|| key.to_string())
    } else {
        LOCALE_EN
            .get(key)
            .map(|s| s.to_string())
            .unwrap_or_else(|| key.to_string())
    }
}
