function readBooleanSetting(key, fallback = false) {
  try {
    const value = localStorage.getItem(key);
    return value === null ? fallback : value === "1";
  } catch {
    return fallback;
  }
}

function writeBooleanSetting(key, value) {
  try { localStorage.setItem(key, value ? "1" : "0"); } catch { /* Settings remain in memory when storage is unavailable. */ }
}

function readNumberSetting(key, fallback, min, max) {
  try {
    const value = Number(localStorage.getItem(key));
    return Number.isInteger(value) && value >= min && value <= max ? value : fallback;
  } catch {
    return fallback;
  }
}

function writeNumberSetting(key, value) {
  try { localStorage.setItem(key, String(value)); } catch { /* Settings remain in memory when storage is unavailable. */ }
}

let groups = [
  {
    id: "development",
    name: "开发组",
    note: "3 个可用 Key",
    keys: [
      {
        id: "key-dev-main",
        name: "Codex 主用",
        masked: "sk-2x…9F2A",
        status: "active",
        models: ["gpt-5.4", "gpt-5.4-mini"],
        created: "2026-08-09",
        quota: "¥ 820.60",
      },
      {
        id: "key-dev-ci",
        name: "CI 自动化",
        masked: "sk-2x…71CC",
        status: "active",
        models: ["gpt-5.4-mini"],
        created: "2026-08-02",
        quota: "¥ 256.00",
      },
      {
        id: "key-dev-lab",
        name: "实验环境",
        masked: "sk-2x…5A7D",
        status: "active",
        models: ["gpt-5.4", "gpt-5.4-mini"],
        created: "2026-07-27",
        quota: "¥ 102.85",
      },
    ],
  },
  {
    id: "design",
    name: "设计组",
    note: "2 个可用 Key",
    keys: [
      {
        id: "key-design-main",
        name: "内容与设计",
        masked: "sk-2x…6CB0",
        status: "active",
        models: ["gpt-5.4", "gpt-5.4-mini"],
        created: "2026-08-05",
        quota: "¥ 183.40",
      },
      {
        id: "key-design-readonly",
        name: "只读校验",
        masked: "sk-2x…0D31",
        status: "paused",
        models: ["gpt-5.4-mini"],
        created: "2026-07-18",
        quota: "¥ 0.00",
      },
    ],
  },
  {
    id: "personal",
    name: "个人组",
    note: "1 个可用 Key",
    keys: [
      {
        id: "key-personal",
        name: "个人试用",
        masked: "sk-2x…AF39",
        status: "active",
        models: ["gpt-5.4-mini"],
        created: "2026-08-10",
        quota: "¥ 76.50",
      },
    ],
  },
];

const state = {
  view: "providers",
  authenticated: false,
  demoMode: false,
  authLoading: false,
  authError: null,
  twoFactorRequired: false,
  rememberLogin: true,
  rememberedEmail: "",
  rememberedPassword: "",
  autoLoginAttempted: false,
  loginModalOpen: false,
  xapiTab: "overview",
  providers: [],
  currentProviderId: null,
  providerFormOpen: false,
  providerForm: { editing: null, name: "", baseUrl: "", apiKey: "", model: "", wireApi: "responses", accessMode: "pureApi", websiteUrl: "", notes: "", probing: false, probeResult: null },
  captcha: {
    enabled: false,
    provider: null,
    appId: null,
    region: "cn",
    proof: null,
    loading: false,
    error: null,
  },
  health: null,
  selectedGroupId: "development",
  selectedKeyId: "key-dev-main",
  currentMode: "official",
  account: {
    email: "wen***@2xapi.com",
    name: "2xapi 工作账号",
    tenant: "个人空间",
  },
  lastVerified: "尚未验证",
  repair: {
    previewed: false,
    progress: 0,
    result: "尚未运行历史会话修复。",
    auto: readBooleanSetting("2xapi.autoPreviewHistory", false),
    inspection: null,
  },
  lastConfigBackupPath: null,
  retentionDays: readNumberSetting("2xapi.backupRetentionDays", 30, 1, 3650),
  backups: [
    {
      id: "backup-1",
      title: "应用 2xapi 配置前",
      kind: "配置备份",
      path: "2026-08-11T10-24-05",
      date: "今天 10:24",
    },
    {
      id: "backup-2",
      title: "历史会话修复前",
      kind: "会话备份",
      path: "2026-08-10T21-08-42",
      date: "昨天 21:08",
    },
    {
      id: "backup-3",
      title: "官方配置快照",
      kind: "配置备份",
      path: "2026-08-09T09-33-16",
      date: "8 月 9 日",
    },
  ],
  toasts: [],
};

const navItems = [
  ["xapi", "2xapi", "⌘", "https://2xapi.com"],
  ["providers", "供应商", "◈"],
  ["repair", "历史修复", "↺"],
  ["backups", "备份", "▣"],
  ["settings", "设置", "⚙"],
];

const app = document.querySelector("#app");

function selectedGroup() {
  return resolveSelectedGroup(groups, state.selectedGroupId);
}

function selectedKey() {
  return resolveSelectedKey(groups, state.selectedGroupId, state.selectedKeyId) || { id: "", name: "未选择 Key", masked: "-", status: "inactive", models: [], created: "-", quota: "-" };
}

function firstUsableGroup(groups) {
  const list = Array.isArray(groups) ? groups : [];
  return list.find((group) => (group.keys || []).some((key) => key.status === "active"))
    || list.find((group) => (group.keys || []).length > 0)
    || list[0]
    || null;
}

function resolveSelectedGroup(groups, selectedGroupId) {
  const list = Array.isArray(groups) ? groups : [];
  const byId = list.find((group) => group.id === selectedGroupId);
  return byId || firstUsableGroup(list) || { id: "", name: "暂无分组", note: "0 个可用 Key", keys: [] };
}

function resolveSelectedKey(groups, selectedGroupId, selectedKeyId) {
  const group = resolveSelectedGroup(groups, selectedGroupId);
  const keys = (group.keys || []).filter((key) => key && key.id);
  if (keys.length === 0) return null;
  return keys.find((key) => String(key.id) === String(selectedKeyId)) || keys[0] || null;
}

function currentModeLabel() {
  return state.currentMode === "platform" ? "2xapi 平台 API" : "官方 Codex";
}

function escapeHtml(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

function statusLabel(status) {
  return ({ active: "可用", inactive: "已停用", disabled: "已停用", expired: "已过期", quota_exhausted: "配额用尽" })[status] || "不可用";
}

function formatQuota(key) {
  if (typeof key?.quota === "string" && key.quota.trim() && !/^\s*-?\d+(?:\.\d+)?\s*$/.test(key.quota)) return key.quota.trim();
  const limit = Number(key?.quota);
  const used = Number(key?.quotaUsed);
  if (!Number.isFinite(limit)) return "由平台返回";
  if (limit <= 0) return Number.isFinite(used) && used > 0 ? `${used.toFixed(2)} USD / 不限额` : "不限额";
  return `${Number.isFinite(used) ? used.toFixed(2) : "0.00"} / ${limit.toFixed(2)} USD`;
}

function toast(message, type = "success") {
  state.toasts.push({ id: Date.now(), message, type });
  renderPage();
  window.setTimeout(() => {
    state.toasts.shift();
    renderPage();
  }, 3400);
}

let captchaScriptPromise = null;

function loadExternalScript(source, globalName) {
  if (window[globalName]) return Promise.resolve(window[globalName]);
  if (captchaScriptPromise) return captchaScriptPromise;
  captchaScriptPromise = new Promise((resolve, reject) => {
    const existing = document.querySelector(`script[src="${source}"]`);
    if (existing) {
      existing.addEventListener("load", () => resolve(window[globalName]), { once: true });
      existing.addEventListener("error", () => reject(new Error("验证码组件加载失败")), { once: true });
      return;
    }
    const script = document.createElement("script");
    script.src = source;
    script.async = true;
    script.onload = () => window[globalName] ? resolve(window[globalName]) : reject(new Error("验证码组件不可用"));
    script.onerror = () => reject(new Error("验证码组件加载失败"));
    document.head.appendChild(script);
  }).finally(() => { captchaScriptPromise = null; });
  return captchaScriptPromise;
}

async function requestCaptchaProof() {
  if (!state.captcha.enabled) return true;
  if (state.captcha.proof) return true;
  state.captcha.loading = true;
  state.captcha.error = null;
  renderPage();
  try {
    if (state.captcha.provider !== "tencent") throw new Error("当前验证码提供商暂不支持，请联系管理员");
    await loadExternalScript("https://turing.captcha.qcloud.com/TJCaptcha.js", "TencentCaptcha");
    const proof = await new Promise((resolve, reject) => {
      let challenge;
      const callback = (result) => {
        if (result?.ret === 0 && result.ticket && result.randstr && !String(result.ticket).startsWith("trerror_")) {
          resolve({ ticket: String(result.ticket), randstr: String(result.randstr) });
        } else {
          reject(new Error("安全验证未通过，请重试"));
        }
        try { challenge?.destroy?.(); } catch { /* The SDK may not expose destroy in older versions. */ }
      };
      try {
        challenge = new window.TencentCaptcha(String(state.captcha.appId), callback, { userLanguage: "zh-cn" });
        challenge.show();
      } catch (error) {
        reject(new Error(error?.message || "安全验证无法打开"));
      }
    });
    state.captcha.proof = proof;
    state.captcha.error = null;
    toast("安全验证已完成。", "success");
    return true;
  } catch (error) {
    state.captcha.proof = null;
    state.captcha.error = error.message || "安全验证失败";
    toast(state.captcha.error, "warning");
    return false;
  } finally {
    state.captcha.loading = false;
    renderPage();
  }
}

function renderShell(content) {
  const modeClass = state.currentMode === "platform" ? "platform" : "official";
  const modeText = state.currentMode === "platform" ? "平台 API 生效中" : "官方登录模式";
  return `
    <div class="app-shell">
      <aside class="sidebar">
        <div class="brand">
          <div class="brand-mark">2×</div>
          <div class="brand-copy"><strong>2xapi</strong><span>Codex 控制台</span></div>
        </div>
        <div class="nav-label">工作区</div>
        <nav class="nav-list" aria-label="主导航">
          ${navItems
            .map((item) => {
              const [id, label, icon, href] = item;
              if (href) return `<button class="nav-item" data-action="open-external" data-href="${href}" type="button" title="${label}"><span class="nav-icon" aria-hidden="true">${icon}</span><span>${label}</span></button>`;
              return `<button class="nav-item ${state.view === id ? "is-active" : ""}" data-nav="${id}" type="button" title="${label}"><span class="nav-icon" aria-hidden="true">${icon}</span><span>${label}</span></button>`;
            })
            .join("")}
        </nav>
        <div class="sidebar-foot">
          <div class="connection-card">
            <div class="eyebrow">2xapi</div>
            <div class="connection-row"><i class="signal"></i><span>${state.demoMode ? "演示数据" : state.health?.ok ? "服务连接正常" : "正在检查服务"}</span></div>
          </div>
        </div>
      </aside>
      <main class="main-area">
        <header class="topbar">
          <div class="crumbs"><span>2xapi</span><span>/</span><span class="crumb-current">${navItems.find(([id]) => id === state.view)?.[1] || "概览"}</span></div>
          <div class="top-actions">
            <span class="mode-badge ${modeClass}"><i class="signal"></i>${modeText}</span>
            ${state.authenticated ? `<button class="account-button" data-action="logout" type="button" title="点击退出登录">
              <span class="avatar">2X</span>
              <span class="account-copy"><strong>${escapeHtml(state.account.email)}</strong><span>${escapeHtml(state.account.tenant)}</span></span>
            </button>` : `<button class="btn primary" data-action="open-login" type="button">登录 2xapi</button>`}
          </div>
        </header>
        <section class="content">${content}</section>
      </main>
    </div>
    <div class="toast-stack" aria-live="polite">
      ${state.toasts
        .map((item) => `<div class="toast ${item.type === "warning" ? "warning" : ""}"><span>${item.type === "warning" ? "!" : "✓"}</span><span>${escapeHtml(item.message)}</span></div>`)
        .join("")}
    </div>`;
}

function renderLogin() {
  const captcha = state.captcha;
  const captchaUnavailable = Boolean(captcha.error && !captcha.enabled);
  const authError = state.authError ? `<div class="login-error" role="alert">${escapeHtml(state.authError)}</div>` : "";
  const captchaPanel = captcha.enabled || captcha.error ? `<div class="captcha-panel"><div class="captcha-title">登录安全验证</div><div class="captcha-row"><span class="field-note">${captchaUnavailable ? "暂时无法读取安全配置，仍可直接尝试登录。" : "2xapi 要求完成一次安全验证，验证结果只保存在本次登录内存中。"}</span><button class="btn ghost" data-action="${captchaUnavailable ? "refresh-captcha" : "verify-captcha"}" type="button" ${captcha.loading ? "disabled" : ""}>${captcha.loading ? "读取中..." : captchaUnavailable ? "重试" : captcha.proof ? "已完成" : "开始验证"}</button></div>${captcha.error ? `<div class="field-note warning-text">${escapeHtml(captcha.error)}</div>` : ""}</div>` : "";
  return `<main class="login-shell"><section class="login-panel"><div class="brand"><div class="brand-mark">2×</div><div class="brand-copy"><strong>2xapi</strong><span>Codex 控制台</span></div></div><div class="eyebrow">2xapi Account</div><h1>登录平台账号</h1><p class="subtle">登录只用于读取你账号下的分组 Key，不会注销 Codex 官方登录。</p>${state.twoFactorRequired ? `<form data-2fa-form class="form-stack"><div class="field"><label for="totp-code">验证码</label><input id="totp-code" name="code" inputmode="numeric" autocomplete="one-time-code" required></div>${authError}<button class="btn primary" type="submit" ${state.authLoading ? "disabled" : ""}>${state.authLoading ? "验证中..." : "完成登录"}</button><button class="btn ghost" data-action="cancel-2fa" type="button">返回</button></form>` : `<form data-login-form class="form-stack"><div class="field"><label for="login-email">邮箱</label><input id="login-email" name="email" type="email" autocomplete="username" value="${escapeHtml(state.rememberedEmail)}" required></div><div class="field"><label for="login-password">密码</label><input id="login-password" name="password" type="password" autocomplete="current-password" value="${escapeHtml(state.rememberedPassword)}" required></div><label class="field remember-field"><input type="checkbox" name="remember" ${state.rememberLogin ? "checked" : ""}><span>记住账号密码（下次打开自动登录）</span></label>${captchaPanel}${authError}<button class="btn primary" type="submit" ${state.authLoading ? "disabled" : ""}>${state.authLoading ? "登录中..." : "登录 2xapi"}</button></form>`}<p class="field-note">${state.rememberLogin ? "已勾选记住密码：账号密码保存在本机（仅本机可读），下次打开自动登录。" : "密码只在登录请求期间使用，不写入本地文件。"}官方 Codex 登录保持不变。</p></section></main>`;
}

function pageHeading(eyebrow, title, detail, actions = "") {
  return `
    <div class="page-heading">
      <div><div class="eyebrow">${eyebrow}</div><h1>${title}</h1><p class="subtle">${detail}</p></div>
      ${actions ? `<div class="button-row">${actions}</div>` : ""}
    </div>`;
}

function renderOverview() {
  const key = selectedKey();
  const group = selectedGroup();
  const platform = state.currentMode === "platform";
  const historyCount = state.repair.inspection?.state?.total ?? "-";
  return `
    ${pageHeading("Codex Provider", "运行概览", "管理官方登录与 2xapi 平台调用", `<button class="btn ghost" data-action="goto-xapi-keys" type="button">选择分组 Key <span>→</span></button>`)}
    <div class="overview-grid">
      <section class="workspace-panel">
        <div class="workspace-top">
          <div>
            <span class="tag ${platform ? "active" : "official"}">${platform ? "平台模式" : "官方模式"}</span>
            <div class="mode-title">${platform ? "2xapi 平台 API" : "官方 Codex 登录"}</div>
            <div class="provider-line"><span>当前 Provider</span><code>${platform ? "custom (2xapi)" : "openai"}</code></div>
          </div>
          <div class="button-row">
            ${platform ? `<button class="btn ghost" data-action="restore-official" type="button">恢复官方模式</button>` : `<button class="btn primary" data-action="apply-platform" type="button" ${key.status !== "active" ? "disabled" : ""}>${key.id ? `应用 ${escapeHtml(key.name)}` : "请先选择可用 Key"}</button>`}
          </div>
        </div>
        <div class="workspace-divider"></div>
        <div class="compact-stats">
          <div><span>已选分组</span><strong>${escapeHtml(group.name)}</strong></div>
          <div><span>当前 Key</span><strong>${platform ? (key.id ? escapeHtml(key.masked) : "该分组无可用 Key") : "未应用"}</strong></div>
          <div><span>最近验证</span><strong>${escapeHtml(state.lastVerified)}</strong></div>
        </div>
      </section>
      <div class="side-stack">
        <section class="side-panel">
          <div class="panel-header"><h2>快速应用</h2><span class="tag neutral">安全存储</span></div>
          <div class="form-stack">
            <div class="field"><label for="quick-group">Key 分组</label><select id="quick-group" data-select-group>${groups.map((item) => `<option value="${escapeHtml(item.id)}" ${item.id === group.id ? "selected" : ""}>${escapeHtml(item.name)}</option>`).join("")}</select></div>
            <div class="field"><label for="quick-key">API Key</label><select id="quick-key" data-select-key>${group.keys.map((item) => `<option value="${escapeHtml(item.id)}" ${item.id === key.id ? "selected" : ""}>${escapeHtml(item.name)} · ${escapeHtml(item.masked)}</option>`).join("")}</select><div class="field-note">凭据保存到系统安全存储，并按第三方 provider 应用</div></div>
            <button class="btn primary" data-action="apply-platform" type="button" ${key.status !== "active" ? "disabled" : ""}>验证并应用到 Codex</button>
          </div>
        </section>
        <section class="side-panel">
          <div class="panel-header"><h2>本机状态</h2><span class="tag active">已检查</span></div>
          <div class="status-list">
            <div class="status-row"><span>官方登录</span><strong>${state.health?.officialAuthPresent ? "已保留" : "未检测到文件"}</strong></div>
            <div class="status-row"><span>config.toml</span><strong>${state.health?.writeMode === "write-enabled" ? "写入已启用" : "预览模式"}</strong></div>
            <div class="status-row"><span>第三方凭据</span><strong>系统安全存储</strong></div>
          </div>
        </section>
      </div>
    </div>
    <div class="summary-strip">
      <section class="stat-card"><span>可用分组</span><strong>${groups.length}</strong><div class="delta">账号授权范围内</div></section>
      <section class="stat-card"><span>可用 Key</span><strong>${groups.flatMap((item) => item.keys).filter((item) => item.status === "active").length}</strong><div class="delta">状态正常</div></section>
      <section class="stat-card"><span>配置备份</span><strong>${state.backups.length}</strong><div class="delta">可随时恢复</div></section>
      <section class="stat-card"><span>历史会话</span><strong>${historyCount}</strong><div class="delta">本机索引已读取</div></section>
    </div>
    <section class="table-panel">
      <div class="panel-header"><h2>最近配置活动</h2><button class="btn ghost" data-nav="backups" type="button">查看备份</button></div>
      <div class="table-wrap"><table><thead><tr><th>时间</th><th>动作</th><th>Provider</th><th>结果</th></tr></thead><tbody>
        <tr><td>今天 10:24</td><td>已创建配置快照</td><td><span class="mono">openai</span></td><td><span class="tag active">完成</span></td></tr>
        <tr><td>今天 10:21</td><td>读取会话索引</td><td><span class="mono">local</span></td><td><span class="tag active">完成</span></td></tr>
        <tr><td>昨天 21:08</td><td>历史修复预览</td><td><span class="mono">2xapi</span></td><td><span class="tag warning">待确认</span></td></tr>
      </tbody></table></div>
    </section>`;
}

function renderKeys() {
  const group = selectedGroup();
  const key = selectedKey();
  return `
    ${pageHeading("Account Keys", "分组 API Key", "只显示当前 2xapi 账号有权限访问的凭据", `<button class="btn primary" data-action="apply-platform" type="button" ${key.status !== "active" ? "disabled" : ""}>验证并应用</button>`)}
    <div class="groups-layout">
      <section class="side-panel"><div class="panel-header"><h2>分组</h2><span class="tag neutral">${groups.length}</span></div><div class="group-list">${groups.map((item) => `<button class="group-item ${item.id === group.id ? "is-selected" : ""}" data-group="${escapeHtml(item.id)}" type="button"><strong>${escapeHtml(item.name)}</strong><span>${escapeHtml(item.note)}</span></button>`).join("")}</div></section>
      <section class="table-panel">
        <div class="panel-header"><div><h2>${escapeHtml(group.name)}</h2><p class="subtle">选中 Key 会在应用后作为 Codex 的当前平台凭据。</p></div><span class="tag active">${group.keys.filter((item) => item.status === "active").length} 可用</span></div>
        <div class="table-wrap"><table><thead><tr><th></th><th>Key</th><th>状态</th><th>模型/平台</th><th>配额</th><th>创建时间</th></tr></thead><tbody>
          ${group.keys.map((item) => `<tr><td><input class="table-select" data-key-radio="${escapeHtml(item.id)}" type="radio" name="key" ${item.id === key.id ? "checked" : ""} aria-label="选择 ${escapeHtml(item.name)}"></td><td><div class="key-name">${escapeHtml(item.name)}</div><span class="mono">${escapeHtml(item.masked)}</span></td><td><span class="tag ${item.status === "active" ? "active" : "warning"}">${statusLabel(item.status)}</span></td><td><div class="model-list">${item.models.map((model) => `<span class="model-pill">${escapeHtml(model)}</span>`).join("")}</div></td><td>${escapeHtml(formatQuota(item))}</td><td>${escapeHtml(item.created)}</td></tr>`).join("")}
        </tbody></table></div>
      </section>
    </div>
    <section class="table-panel" style="margin-top:18px">
      <div class="panel-header"><h2>配置差异</h2><span class="tag neutral">不会修改官方登录</span></div>
      ${renderConfigDiff()}
    </section>`;
}

function renderConfigDiff() {
  const key = selectedKey();
  const helperMode = state.health?.tokenMode === "helper";
  return `<div class="diff-grid"><div class="diff-column"><h3>当前</h3><div class="diff-line"><span>model_provider</span><span>${state.currentMode === "platform" ? "custom" : "openai"}</span></div><div class="diff-line"><span>auth.json</span><span>保持不变</span></div><div class="diff-line"><span>凭据来源</span><span>${state.currentMode === "platform" ? "平台 Key" : "官方登录"}</span></div></div><div class="diff-column"><h3>应用后</h3><div class="diff-line"><span>model_provider</span><span class="new">custom</span></div><div class="diff-line"><span>${helperMode ? "auth.command" : "requires_openai_auth"}</span><span class="new">${helperMode ? "系统凭据 helper" : "false"}</span></div><div class="diff-line"><span>selected_key</span><span class="new">${escapeHtml(key?.masked || "未选择")}</span></div><div class="diff-line"><span>${helperMode ? "Key 存储" : "experimental_bearer_token"}</span><span class="new">${helperMode ? "Keychain / Credential Manager" : "已脱敏"}</span></div></div></div>`;
}

function renderRepair() {
  const { repair } = state;
  const inspection = repair.inspection;
  const canRepair = repair.previewed && inspection?.repairable === true;
  const total = inspection?.state?.total ?? "未扫描";
  const active = inspection?.state?.active ?? "未扫描";
  const archived = inspection?.state?.archived ?? "未扫描";
  const missingCatalog = inspection?.catalog?.missingEntries ?? (repair.previewed ? "待检查" : "未扫描");
  const providerMismatches = inspection?.catalog?.providerMismatches ?? (repair.previewed ? "待检查" : "未扫描");
  const missingRollouts = inspection?.state?.missingRollouts ?? (repair.previewed ? "待检查" : "未扫描");
  const sessionIndexDuplicates = inspection?.sessionIndex?.duplicates ?? (repair.previewed ? "待检查" : "未扫描");
  const invalidSessionIndex = inspection?.sessionIndex?.invalidLines ?? 0;
  return `
    ${pageHeading("Session Integrity", "历史会话修复", "先预览，再备份，再写入。修复期间需关闭 Codex。", `<button class="btn ghost" data-action="refresh-sessions" type="button">刷新会话</button><button class="btn ${canRepair ? "primary" : "ghost"}" data-action="${repair.previewed ? "repair" : "preview-repair"}" type="button" ${repair.previewed && !canRepair ? "disabled" : ""}>${!repair.previewed ? "预览修复" : canRepair ? "立即修复历史会话" : "当前格式只读"}</button>`)}
    <div class="repair-grid">
      <section class="repair-panel">
        <div class="panel-header"><h2>本机会话库</h2><span class="tag ${inspection?.supported ? "active" : "warning"}">${inspection?.supported ? "已识别" : "只读诊断"}</span></div>
        <div class="path-row"><span>▸</span><span class="mono">${escapeHtml(inspection?.paths?.catalogDb || "~/.codex/sqlite/codex-dev.db")}</span></div>
        <div class="repair-stats"><div class="repair-number"><span>全部会话</span><strong>${total}</strong></div><div class="repair-number"><span>未归档</span><strong>${active}</strong></div><div class="repair-number"><span>已归档</span><strong>${archived}</strong></div></div>
        <div class="panel-header"><h2>修复目标</h2><span class="tag neutral">${repair.previewed ? "已预览" : "等待预览"}</span></div>
        <div class="repair-preview">
          <div class="preview-row"><span>Provider 归属标记</span><strong>${providerMismatches} 项不一致</strong></div>
          <div class="preview-row"><span>SQLite catalog</span><strong>${missingCatalog} 项待补齐</strong></div>
          <div class="preview-row"><span>Legacy session index</span><strong>${sessionIndexDuplicates} 项重复${invalidSessionIndex ? `，${invalidSessionIndex} 行无效` : ""}</strong></div>
          <div class="preview-row"><span>Rollout 对账</span><strong>${missingRollouts} 项缺失</strong></div>
        </div>
      </section>
      <section class="repair-panel">
        <div class="panel-header"><h2>修复进度</h2><strong>${repair.progress}%</strong></div>
        <div class="progress-track" style="margin-top:18px"><div class="progress-bar" style="width:${repair.progress}%"></div></div>
        <p class="subtle">${escapeHtml(repair.result)}</p>
        <div class="workspace-divider"></div>
        <div class="toggle-row"><div class="toggle-copy"><strong>启动时自动预览</strong><span>只做诊断，不自动写入</span></div><label class="switch"><input data-auto-repair type="checkbox" ${repair.auto ? "checked" : ""}><span class="slider"></span></label></div>
        <div class="button-row" style="margin-top:17px"><button class="btn ghost" data-action="preview-repair" type="button">重新预览</button><button class="btn danger" data-action="repair" type="button" ${canRepair ? "" : "disabled"}>创建备份并修复</button></div>
      </section>
    </div>
    <section class="table-panel" style="margin-top:18px"><div class="panel-header"><h2>安全检查</h2><span class="tag active">默认非破坏性</span></div><div class="status-list"><div class="status-row"><span>Codex 进程</span><strong>已关闭后可执行</strong></div><div class="status-row"><span>写入策略</span><strong>SQLite 事务，session index 原子去重</strong></div><div class="status-row"><span>Rollout 文件</span><strong>只读对账，不重写会话内容</strong></div><div class="status-row"><span>回滚来源</span><strong>数据库与 session index 修复前快照</strong></div></div></section>`;
}

function renderBackups() {
  return `
    ${pageHeading("Recovery", "备份与恢复", "每次应用配置和历史会话修复都会创建可校验的本地快照", `<button class="btn ghost" data-action="new-backup" type="button">创建配置快照</button>`)}
    <section class="backup-panel"><div class="panel-header"><div><h2>本地备份</h2><p class="subtle">恢复只处理应用托管的配置和修复快照，不覆盖官方登录凭据。</p></div><span class="tag active">${state.backups.length} 个可用</span></div><div class="backup-list">${state.backups.map((backup) => `<div class="backup-row"><div><strong>${escapeHtml(backup.title)}</strong><span>${escapeHtml(backup.path)}</span></div><span class="backup-kind">${escapeHtml(backup.kind)}<br>${escapeHtml(backup.date)}</span><button class="btn ghost" data-restore-backup="${escapeHtml(backup.id)}" type="button">恢复此版本</button></div>`).join("")}</div></section>
    <section class="settings-section" style="margin-top:18px"><div class="panel-header"><h2>恢复规则</h2><span class="tag neutral">安全默认值</span></div><div class="status-list" style="margin-top:17px"><div class="status-row"><span>auth.json</span><strong>始终保留</strong></div><div class="status-row"><span>平台 Key</span><strong>从系统凭据存储读取</strong></div><div class="status-row"><span>未知文件</span><strong>不自动覆盖</strong></div></div></section>`;
}

function renderSettings() {
  return `
    ${pageHeading("Local Settings", "设置", "本机路径、凭据 helper 和会话修复策略", "")}
    <div class="account-layout">
      <section class="settings-section"><div class="panel-header"><h2>Codex 集成</h2><span class="tag active">${state.health?.codexRunning ? "需先退出 Codex" : "可写入"}</span></div><div class="key-value-list"><div class="key-value-row"><span>配置文件</span><strong class="mono">${escapeHtml(state.health?.configPath || "~/.codex/config.toml")}</strong></div><div class="key-value-row"><span>当前 Provider</span><strong>${escapeHtml(state.health?.provider?.providerId || "openai")}</strong></div><div class="key-value-row"><span>官方凭据</span><strong>保留，不由本工具写入</strong></div><div class="key-value-row"><span>平台 Key</span><strong>系统安全存储</strong></div><div class="key-value-row"><span>凭据模式</span><strong>${state.health?.tokenMode === "helper" ? "系统 helper（推荐）" : "Raw bearer（兼容模式）"}</strong></div></div></section>
      <section class="settings-section"><div class="panel-header"><h2>默认行为</h2><span class="tag neutral">本机</span></div><div class="toggle-row"><div class="toggle-copy"><strong>写入前创建备份</strong><span>无法关闭</span></div><label class="switch"><input type="checkbox" checked disabled><span class="slider"></span></label></div><div class="toggle-row"><div class="toggle-copy"><strong>启动时自动预览</strong><span>检测索引异常，但不自动写入</span></div><label class="switch"><input data-auto-repair type="checkbox" ${state.repair.auto ? "checked" : ""}><span class="slider"></span></label></div><div class="toggle-row"><div class="toggle-copy"><strong>自动重启 Codex</strong><span>当前版本保持关闭</span></div><label class="switch"><input type="checkbox" disabled><span class="slider"></span></label></div><div class="workspace-divider"></div><div class="retention-control"><label for="retention-days"><strong>备份保留天数</strong><span>只清理本工具创建的过期备份</span></label><input id="retention-days" data-retention-days type="number" min="1" max="3650" step="1" value="${state.retentionDays}"><button class="btn ghost" data-action="prune-backups" type="button">清理过期备份</button></div></section>
    </div>`;
}

function renderAccount() {
  return `
    ${pageHeading("2xapi Account", "账户", "当前会话仅用于读取账号下的分组和 Key", `<button class="btn ghost" data-action="logout" type="button">退出登录</button>`)}
    <div class="account-layout">
      <section class="account-panel"><div class="account-summary"><span class="avatar">2X</span><div><h2>${escapeHtml(state.account.name)}</h2><p class="subtle">${escapeHtml(state.account.email)}</p></div><span class="tag active" style="margin-left:auto">已授权</span></div><div class="key-value-list"><div class="key-value-row"><span>空间</span><strong>${escapeHtml(state.account.tenant)}</strong></div><div class="key-value-row"><span>授权范围</span><strong>分组、Key 元数据、选中 Key 凭据</strong></div><div class="key-value-row"><span>会话状态</span><strong>本机安全会话</strong></div><div class="key-value-row"><span>当前模式</span><strong>${currentModeLabel()}</strong></div></div></section>
      <section class="account-panel"><div class="panel-header"><h2>连接状态</h2><span class="tag active">正常</span></div><div class="status-list" style="margin-top:18px"><div class="status-row"><span>2xapi 账户接口</span><strong>已连接</strong></div><div class="status-row"><span>Key 分组接口</span><strong>${groups.length} 个分组</strong></div><div class="status-row"><span>凭据读取权限</span><strong>仅选中 Key</strong></div><div class="status-row"><span>Token 过期时间</span><strong>01:52:34 后</strong></div></div><div class="workspace-divider"></div><button class="btn primary" data-action="refresh-token" type="button">刷新授权状态</button></section>
    </div>`;
}

function renderProviders() {
  const list = state.providers || [];
  const officialActive = state.health?.provider?.providerId !== "custom";
  const f = state.providerForm;
  const modeLabel = () => "第三方 API";
  const AVATAR_COLORS = ["#52d5ad", "#89b9ff", "#f0bb63", "#c792ea", "#fb8b78", "#82e9de", "#f78ba8", "#a9f1cf"];
  const avatarColor = (name) => {
    let h = 0;
    for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) >>> 0;
    return AVATAR_COLORS[h % AVATAR_COLORS.length];
  };
  const avatar = (name, letter, active) => `<span class="pv-avatar" style="background:${avatarColor(name)}">${escapeHtml(letter || name.charAt(0).toUpperCase())}</span>`;

  const officialCard = `<div class="pv-card ${officialActive ? "is-active" : ""}">
    ${avatar("official", "O", officialActive)}
    <div class="pv-info">
      <div class="pv-title"><strong>官方登录</strong> <span class="tag neutral">内置</span>${officialActive ? '<span class="tag active">当前</span>' : ""}</div>
      <div class="pv-meta"><span class="pv-url">Codex 官方 OpenAI 登录</span></div>
      <div class="pv-subtle">恢复官方 provider（从切换前备份还原 config 与 auth）</div>
    </div>
    <div class="pv-actions">
      <button class="btn ${officialActive ? "ghost" : "primary"}" data-action="apply-official" type="button" ${officialActive ? "disabled" : ""}>${officialActive ? "当前模式" : "启用"}</button>
    </div>
  </div>`;

  const cards = list.map((p) => {
    const active = state.currentProviderId === p.id;
    return `<div class="pv-card ${active ? "is-active" : ""}">
      ${avatar(p.name, p.name.charAt(0).toUpperCase(), active)}
      <div class="pv-info">
        <div class="pv-title">
          <strong>${escapeHtml(p.name)}</strong>
          <span class="tag neutral">${modeLabel()}</span>
          ${active ? '<span class="tag active">当前</span>' : ""}
        </div>
        <div class="pv-meta"><a class="pv-url" href="${escapeHtml(p.websiteUrl || p.baseUrl)}" target="_blank" rel="noopener">${escapeHtml(p.baseUrl)}</a>${p.model ? `<span class="pv-dot">·</span><span>${escapeHtml(p.model)}</span>` : ""}${p.wireApi ? `<span class="pv-dot">·</span><span>${escapeHtml(p.wireApi)}</span>` : ""}</div>
        <div class="pv-subtle">${escapeHtml(p.apiKeyMasked || "无 key")}${p.notes ? ` · ${escapeHtml(p.notes)}` : ""}</div>
      </div>
      <div class="pv-actions">
        <button class="btn ${active ? "ghost" : "primary"}" data-apply-provider="${escapeHtml(p.id)}" type="button" ${active ? "disabled" : ""}>${active ? "已启用" : "启用"}</button>
        <button class="btn ghost icon-only" data-edit-provider="${escapeHtml(p.id)}" type="button" title="编辑">✎</button>
        <button class="btn ghost icon-only" data-delete-provider="${escapeHtml(p.id)}" type="button" title="删除">✕</button>
      </div>
    </div>`;
  }).join("");

  const empty = !list.length ? `<p class="subtle pv-empty">还没有自定义供应商，点击右上角「添加供应商」创建。</p>` : "";

  const modal = state.providerFormOpen ? renderProviderModal(f) : "";

  return `
    ${pageHeading("Providers", "供应商管理", "官方登录 / 第三方 API 两种接入方式", `${state.authenticated ? `<button class="btn ghost" data-action="import-provider-from-key" type="button">从 2xapi 账号导入</button>` : ""}<button class="btn ghost" data-action="refresh-providers" type="button">刷新</button><button class="btn primary" data-action="new-provider" type="button">+ 添加供应商</button>`)}
    <div class="pv-list-head"><h2>供应商列表</h2><span class="tag neutral">${list.length + 1} 个</span></div>
    <div class="pv-list">${officialCard}${cards}${empty}</div>
    ${modal}`;
}

function renderProviderModal(f) {
  return `
    <div class="pv-modal-backdrop" data-action="close-provider-modal"></div>
    <div class="pv-modal" role="dialog" aria-modal="true">
      <div class="pv-modal-head">
        <button class="btn ghost icon-only" data-action="close-provider-modal" type="button" title="返回">←</button>
        <h2>${f.editing ? "编辑供应商" : "添加新供应商"}</h2>
      </div>
      <div class="pv-modal-tip"><span>💡</span> 只需填写名称、Base URL 和 API Key，下方会自动写入 Codex 配置。先点「探测端点」可校验连通性并自动填充模型。</div>
      <form data-provider-form class="pv-modal-body form-stack">
        <div class="pv-avatar-row"><span class="pv-avatar lg" style="background:#2d3934">${escapeHtml((f.name || "P").charAt(0).toUpperCase())}</span></div>
        <div class="pv-field-grid">
          <div class="field"><label>供应商名称</label><input name="name" value="${escapeHtml(f.name)}" placeholder="我的供应商" required></div>
          <div class="field"><label>备注（可选）</label><input name="notes" value="${escapeHtml(f.notes)}" placeholder="备注信息"></div>
        </div>
        <div class="field"><label>官网链接（可选）</label><input name="websiteUrl" value="${escapeHtml(f.websiteUrl)}" placeholder="https://provider.example.com"></div>
        <div class="field"><label>API Key</label><input name="apiKey" type="password" value="${escapeHtml(f.apiKey)}" placeholder="${f.editing ? "留空保持不变" : "sk-..."}" ${f.editing ? "" : "required"}><span class="field-hint">自动填充到 auth.json / bearer，无需手动改文件</span></div>
        <div class="field"><label>API 请求地址（Base URL）</label><input name="baseUrl" value="${escapeHtml(f.baseUrl)}" placeholder="https://api.example.com/v1" required></div>
        <div class="pv-field-grid">
          <div class="field"><label>默认模型</label><input name="model" value="${escapeHtml(f.model)}" placeholder="gpt-5.6"></div>
          <input type="hidden" name="accessMode" value="pureApi">
        </div>
        <div class="field"><label>上游协议</label><input type="hidden" name="wireApi" value="responses"><span class="field-hint">Responses API（Codex 0.147+ 仅支持此项）</span></div>
        ${f.probeResult ? `<div class="field-note ${f.probeResult.reachable ? "" : "warning-text"}">${f.probeResult.reachable ? `✓ 端点可达，发现 ${f.probeResult.modelCount} 个模型` : "✗ 端点不可达，请检查地址或网络"}</div>` : ""}
        <div class="pv-modal-foot">
          <button type="button" class="btn ghost" data-action="probe-provider" ${f.probing ? "disabled" : ""}>${f.probing ? "探测中…" : "探测端点"}</button>
          <div class="pv-modal-foot-right"><button type="button" class="btn ghost" data-action="close-provider-modal">取消</button><button type="submit" class="btn primary">${f.editing ? "保存修改" : "添加"}</button></div>
        </div>
      </form>
    </div>`;
}

function renderXapi() {
  const tabs = [["overview", "概览"], ["keys", "API Key"], ["account", "账号"]];
  const tab = state.xapiTab || "overview";
  const tabContent = tab === "keys" ? renderKeys() : tab === "account" ? renderAccount() : renderOverview();
  return `
    <div class="xapi-tabs">
      ${tabs.map(([id, label]) => `<button class="xapi-tab ${tab === id ? "is-active" : ""}" data-xapi-tab="${id}" type="button">${label}</button>`).join("")}
    </div>
    ${tabContent}`;
}

function renderLoginModal() {
  if (!state.loginModalOpen) return "";
  const captcha = state.captcha;
  const captchaUnavailable = Boolean(captcha.error && !captcha.enabled);
  const authError = state.authError ? `<div class="login-error" role="alert">${escapeHtml(state.authError)}</div>` : "";
  const captchaPanel = captcha.enabled || captcha.error ? `<div class="captcha-panel"><div class="captcha-row"><button class="btn ghost" data-action="${captchaUnavailable ? "refresh-captcha" : "verify-captcha"}" type="button" ${captcha.loading ? "disabled" : ""}>${captcha.loading ? "读取中..." : captchaUnavailable ? "重试" : captcha.proof ? "已完成" : "开始验证"}</button></div>${captcha.error ? `<div class="field-note warning-text">${escapeHtml(captcha.error)}</div>` : ""}</div>` : "";
  const form = state.twoFactorRequired
    ? `<form data-2fa-form class="form-stack"><div class="field"><label>验证码</label><input name="code" inputmode="numeric" autocomplete="one-time-code" required></div>${authError}<button class="btn primary" type="submit" ${state.authLoading ? "disabled" : ""}>${state.authLoading ? "验证中..." : "完成登录"}</button></form>`
    : `<form data-login-form class="form-stack"><div class="field"><label>邮箱</label><input name="email" type="email" autocomplete="username" value="${escapeHtml(state.rememberedEmail)}" required></div><div class="field"><label>密码</label><input name="password" type="password" autocomplete="current-password" value="${escapeHtml(state.rememberedPassword)}" required></div><label class="field remember-field"><input type="checkbox" name="remember" ${state.rememberLogin ? "checked" : ""}><span>记住账号密码</span></label>${captchaPanel}${authError}<button class="btn primary" type="submit" ${state.authLoading ? "disabled" : ""}>${state.authLoading ? "登录中..." : "登录"}</button></form>`;
  return `
    <div class="pv-modal-backdrop" data-action="close-login-modal"></div>
    <div class="pv-modal" role="dialog" aria-modal="true">
      <div class="pv-modal-head">
        <h2>登录 2xapi</h2>
        <button class="btn ghost icon-only" data-action="close-login-modal" type="button" title="关闭">✕</button>
      </div>
      <p class="subtle" style="margin:4px 0 16px">登录用于读取账号下的分组 Key，不影响 Codex 官方登录。</p>
      ${form}
    </div>`;
}

function renderPage() {
  const pages = {
    repair: renderRepair,
    backups: renderBackups,
    settings: renderSettings,
    providers: renderProviders,
  };
  app.innerHTML = renderShell((pages[state.view] || pages.providers)()) + renderLoginModal();
  bindEvents();
}

async function applyPlatform() {
  const key = selectedKey();
  if (key.status !== "active") {
    toast("所选 Key 当前不可用，请选择一个状态正常的 Key。", "warning");
    return;
  }
  if (window.codexApi && !state.demoMode) {
    try {
      const result = await window.codexApi.applyKey({ keyId: key.id });
      if (result.backupPath) state.lastConfigBackupPath = result.backupPath;
      if (!result.written) {
        toast("配置预览已生成。当前服务是 dry-run，未写入 Codex；设置 CODEX_DESKTOP_ALLOW_WRITE=1 后可执行真实应用。", "warning");
        return;
      }
      // Verification is advisory (non-blocking): the write succeeds regardless, but if the
      // provider's /models probe failed we surface it so an invalid key isn't mistaken for success.
      const verification = result.verification || {};
      if (verification.verified === false && !verification.skipped) {
        toast(`${key.name} 已写入 Codex 配置（未通过连通性验证：${verification.reason || "provider 未能响应"}。若 Key 无效，Codex 启动时会报错）。重启 Codex 后生效。`, "warning");
      } else {
        toast(`${key.name} 已验证并写入 Codex 配置。重启 Codex 后生效。`);
      }
      return;
    } catch (error) {
      toast(`配置预览失败：${error.message}`, "warning");
      return;
    }
  }
  state.currentMode = "platform";
  state.lastVerified = "刚刚";
  state.backups.unshift({ id: `backup-${Date.now()}`, title: "应用 2xapi 配置前", kind: "配置备份", path: new Date().toISOString().slice(0, 19).replace("T", " "), date: "刚刚" });
  toast(state.demoMode ? `${key.name} 已在演示界面中选中；连接本地服务后才会验证并写入 Codex。` : `${key.name} 已验证并写入 Codex 配置。重启 Codex 后生效。`);
}

async function restoreOfficial() {
  if (window.codexApi && !state.lastConfigBackupPath) {
    toast("没有找到应用平台配置前的备份，无法自动恢复官方模式。", "warning");
    return;
  }
  if (window.codexApi && state.lastConfigBackupPath) {
    try {
      const result = await window.codexApi.restoreConfig({ backupPath: state.lastConfigBackupPath });
      if (!result.written) {
        toast("已生成官方模式恢复预览，当前服务是 dry-run。", "warning");
        return;
      }
    } catch (error) {
      toast(`恢复官方模式失败：${error.message}`, "warning");
      return;
    }
  }
  state.currentMode = "official";
  state.lastVerified = "官方模式";
  toast("已恢复官方 Codex provider，官方登录凭据保持不变。");
}

function resetProviderForm() {
  state.providerForm = { editing: null, name: "", baseUrl: "", apiKey: "", model: "", wireApi: "responses", accessMode: "pureApi", websiteUrl: "", notes: "", probing: false, probeResult: null };
  state.providerFormOpen = false;
}
function openProviderFormNew() {
  state.providerForm = { editing: null, name: "", baseUrl: "", apiKey: "", model: "", wireApi: "responses", accessMode: "pureApi", websiteUrl: "", notes: "", probing: false, probeResult: null };
  state.providerFormOpen = true;
  renderPage();
}
function closeProviderForm() {
  state.providerFormOpen = false;
  renderPage();
}
async function importProviderFromKey() {
  if (!state.authenticated) { toast("请先登录 2xapi 账号", "warning"); return; }
  toast("正在导入账号下所有 Key…");
  try {
    const r = await window.codexApi.importProviderFromKey({});
    toast(`已导入 ${r.importedCount} 个供应商${r.failedCount ? `，${r.failedCount} 个失败` : ""}`);
    await refreshProviders();
  } catch (e) { toast(`导入失败：${e.message}`, "warning"); }
}
function syncProviderFormFromDom() {
  const form = document.querySelector("[data-provider-form]");
  if (!form) return;
  const d = Object.fromEntries(new FormData(form).entries());
  Object.assign(state.providerForm, { name: d.name || "", baseUrl: d.baseUrl || "", apiKey: d.apiKey || "", model: d.model || "", wireApi: d.wireApi || "responses", accessMode: d.accessMode || "pureApi", websiteUrl: d.websiteUrl || "", notes: d.notes || "" });
}
async function refreshProviders() {
  if (!window.codexApi) return;
  try {
    const r = await window.codexApi.listProviders();
    state.providers = r.providers || [];
    state.currentProviderId = r.currentProviderId || null;
  } catch (e) { toast(`加载供应商失败：${e.message}`, "warning"); }
  renderPage();
}
async function handleProviderFormSubmit(form) {
  syncProviderFormFromDom();
  const f = state.providerForm;
  if (!f.name.trim() || !f.baseUrl.trim()) { toast("名称和 Base URL 必填", "warning"); return; }
  if (!f.editing && !f.apiKey.trim()) { toast("新增供应商需要 API Key", "warning"); return; }
  try {
    await window.codexApi.saveProvider({ id: f.editing || undefined, name: f.name, baseUrl: f.baseUrl, apiKey: f.apiKey || undefined, model: f.model, wireApi: f.wireApi, accessMode: f.accessMode, websiteUrl: f.websiteUrl, notes: f.notes });
    toast(f.editing ? "供应商已更新" : "供应商已添加");
    resetProviderForm();
    await refreshProviders();
  } catch (e) { toast(`保存失败：${e.message}`, "warning"); }
}
async function applyCustomProvider(id) {
  try {
    const r = await window.codexApi.applyProvider({ id });
    if (r.verification && r.verification.verified === false && !r.verification.skipped) {
      toast(`已写入（端点探测：${r.verification.reason || "未通过"}）。重启 Codex 后生效。`, "warning");
    } else {
      toast(`已切换到 ${r.provider ? r.provider.name : "供应商"}，重启 Codex 后生效。`);
    }
    state.currentProviderId = id;
    state.lastConfigBackupPath = r.backupPath || state.lastConfigBackupPath;
    await hydrateApiData();
  } catch (e) { toast(`切换失败：${e.message}`, "warning"); }
}
function editProvider(id) {
  const p = state.providers.find((x) => x.id === id);
  if (!p) return;
  state.providerForm = { editing: p.id, name: p.name, baseUrl: p.baseUrl, apiKey: "", model: p.model, wireApi: p.wireApi, accessMode: p.accessMode, websiteUrl: p.websiteUrl, notes: p.notes, probing: false, probeResult: null };
  state.providerFormOpen = true;
  renderPage();
}
async function deleteCustomProvider(id) {
  try {
    await window.codexApi.deleteProvider({ id });
    toast("供应商已删除");
    if (state.providerForm.editing === id) resetProviderForm();
    await refreshProviders();
  } catch (e) { toast(`删除失败：${e.message}`, "warning"); }
}
async function probeProviderForm() {
  syncProviderFormFromDom();
  const f = state.providerForm;
  if (!f.baseUrl.trim() || !f.apiKey.trim()) { toast("探测需要 Base URL 和 API Key", "warning"); return; }
  state.providerForm.probing = true;
  renderPage();
  try {
    const r = await window.codexApi.probeProvider({ baseUrl: f.baseUrl, apiKey: f.apiKey });
    state.providerForm.probeResult = r;
    if (r.reachable && r.models && r.models.length && !state.providerForm.model) state.providerForm.model = r.models[0];
  } catch (e) { state.providerForm.probeResult = { reachable: false, modelCount: 0 }; toast(`探测失败：${e.message}`, "warning"); }
  state.providerForm.probing = false;
  renderPage();
}
async function applyOfficialMode() {
  await restoreOfficial();
  state.currentProviderId = null;
  renderPage();
}

async function refreshSessions() {
  try {
    if (!window.codexApi) throw new Error("本地服务未连接");
    state.repair.inspection = await window.codexApi.inspectHistory();
    state.repair.previewed = false;
    toast(`会话库已刷新，共读取 ${state.repair.inspection.state?.total || 0} 个本地会话。`);
  } catch (error) {
    toast(`刷新会话失败：${error.message}`, "warning");
  }
  renderPage();
}

async function createConfigSnapshot() {
  try {
    if (!window.codexApi) throw new Error("本地服务未连接");
    const result = await window.codexApi.snapshotConfig();
    if (!result.written) {
      toast("已生成配置快照预览，当前服务是 dry-run。", "warning");
      return;
    }
    await hydrateApiData();
    toast("已创建新的配置快照。");
  } catch (error) {
    toast(`创建配置快照失败：${error.message}`, "warning");
  }
}

async function pruneBackups() {
  if (!window.codexApi) {
    toast("演示模式不会删除本地备份。", "warning");
    return;
  }
  const confirmed = window.confirm(`将清理超过 ${state.retentionDays} 天、且由本工具创建的备份。继续吗？`);
  if (!confirmed) return;
  try {
    const result = await window.codexApi.pruneBackups({ retentionDays: state.retentionDays });
    if (result.dryRun) {
      toast(`dry-run：发现 ${result.removed} 个过期备份，未执行删除。`, "warning");
      return;
    }
    const removedIds = new Set(result.removedIds || []);
    state.backups = state.backups.filter((backup) => !removedIds.has(backup.id));
    toast(`已清理 ${result.removed} 个过期备份。`);
    renderPage();
  } catch (error) {
    toast(`清理备份失败：${error.message}`, "warning");
  }
}

async function refreshAuthorization() {
  try {
    await hydrateApiData();
    toast("授权状态已刷新。");
  } catch (error) {
    toast(`刷新授权失败：${error.message}`, "warning");
  }
}

async function previewRepair() {
  if (window.codexApi) {
    try {
      const result = await window.codexApi.previewHistory();
      state.repair.inspection = result;
      state.repair.previewed = true;
      state.repair.progress = 0;
      state.repair.result = result.repairable ? `预览完成：${result.counts.missingCatalogEntries} 项 catalog 待补齐，${result.counts.providerMismatches} 项 Provider 标记不一致，${result.counts.sessionIndexDuplicates} 项 session index 重复。` : result.reason || "当前 Codex 数据库格式不支持写入修复。";
      toast("历史会话修复预览已生成。");
      renderPage();
      return;
    } catch (error) {
      toast(`历史会话扫描失败：${error.message}`, "warning");
      return;
    }
  }
  state.repair.previewed = true;
  state.repair.progress = 0;
  state.repair.result = "预览完成。";
  toast("历史会话修复预览已生成。");
}

async function runRepair() {
  if (!state.repair.previewed) {
    toast("请先运行修复预览。", "warning");
    return;
  }
  if (state.repair.inspection?.repairable !== true) {
    toast(state.repair.inspection?.reason || "当前 Codex 数据库格式只支持诊断，不能写入修复。", "warning");
    return;
  }
  state.repair.progress = 28;
  state.repair.result = "正在创建一致性快照并校验 SQLite schema...";
  renderPage();
  if (window.codexApi) {
    try {
      const result = await window.codexApi.applyHistory();
      state.repair.progress = result.written ? 100 : 0;
      state.repair.result = result.written ? `修复完成：补齐 ${result.counts.missingCatalogEntries} 项 catalog，移除 ${result.sessionIndexRepair?.duplicatesRemoved || 0} 项 session index 重复，备份位于 ${result.backupPath}。` : "修复预览已生成，当前服务是 dry-run，未写入本机会话数据。";
      toast(result.written ? "历史会话修复完成，已创建可回滚快照。" : "历史会话修复仍处于 dry-run。", result.written ? "success" : "warning");
      renderPage();
      return;
    } catch (error) {
      state.repair.result = error.message;
      toast(`历史会话修复失败：${error.message}`, "warning");
      renderPage();
      return;
    }
  }
  state.repair.progress = 100;
  state.repair.result = "修复完成。";
  toast("历史会话修复完成，已创建可回滚快照。");
}

function bindEvents() {
  document.querySelectorAll("[data-login-form]").forEach((form) => {
    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      const data = Object.fromEntries(new FormData(form).entries());
      state.authError = null;
      state.authLoading = true;
      renderPage();
      try {
        if (state.captcha.enabled && !state.captcha.proof) {
          state.authLoading = false;
          renderPage();
          toast("请先完成登录安全验证。", "warning");
          return;
        }
        const result = window.codexApi ? await window.codexApi.login({
          ...data,
          ...(state.captcha.proof ? {
            tencentCaptchaTicket: state.captcha.proof.ticket,
            tencentCaptchaRandstr: state.captcha.proof.randstr,
          } : {}),
        }) : { authenticated: true };
        state.captcha.proof = null;
        if (result.requires2fa) {
          state.twoFactorRequired = true;
          toast("请输入 2FA 验证码。", "warning");
        } else {
          state.authenticated = true;
          state.twoFactorRequired = false;
          state.loginModalOpen = false;
          const wantRemember = data.remember === "on";
          state.rememberLogin = wantRemember;
          if (window.codexApi) {
            try {
              if (wantRemember) await window.codexApi.rememberCredentials({ email: data.email, password: data.password });
              else await window.codexApi.forgetCredentials();
            } catch { /* 记住密码失败不阻塞登录 */ }
          }
          toast("2xapi 账号登录成功。");
          await hydrateApiData();
        }
      } catch (error) {
        if (/CAPTCHA|验证码|安全验证/i.test(`${error.code || ""} ${error.message || ""}`)) state.captcha.proof = null;
        state.authError = `登录失败：${error.message}`;
      } finally {
        state.authLoading = false;
        renderPage();
      }
    });
  });

  document.querySelectorAll("[data-2fa-form]").forEach((form) => {
    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      const data = Object.fromEntries(new FormData(form).entries());
      state.authError = null;
      state.authLoading = true;
      renderPage();
      try {
        await window.codexApi.login2fa(data);
        state.authenticated = true;
        state.twoFactorRequired = false;
        state.loginModalOpen = false;
        await hydrateApiData();
        toast("2xapi 账号登录成功。");
      } catch (error) {
        state.authError = `二次验证失败：${error.message}`;
      } finally {
        state.authLoading = false;
        renderPage();
      }
    });
  });

  document.querySelectorAll('[data-action="cancel-2fa"]').forEach((element) => {
    element.addEventListener("click", async () => {
      if (window.codexApi) await window.codexApi.cancel2fa().catch(() => {});
      state.twoFactorRequired = false;
      state.authError = null;
      renderPage();
    });
  });

  document.querySelectorAll("[data-nav]").forEach((element) => {
    element.addEventListener("click", () => {
      state.view = element.dataset.nav;
      renderPage();
    });
  });

  document.querySelectorAll("[data-xapi-tab]").forEach((element) => {
    element.addEventListener("click", () => {
      state.xapiTab = element.dataset.xapiTab;
      renderPage();
    });
  });

  document.querySelectorAll("[data-group]").forEach((element) => {
    element.addEventListener("click", () => {
      state.selectedGroupId = element.dataset.group;
      state.selectedKeyId = selectedGroup().keys[0]?.id || "";
      renderPage();
    });
  });

  document.querySelectorAll("[data-key-radio]").forEach((element) => {
    element.addEventListener("change", () => {
      state.selectedKeyId = element.dataset.keyRadio;
      renderPage();
    });
  });

  document.querySelectorAll("[data-select-group]").forEach((element) => {
    element.addEventListener("change", (event) => {
      state.selectedGroupId = event.target.value;
      state.selectedKeyId = selectedGroup().keys[0]?.id || "";
      renderPage();
    });
  });

  document.querySelectorAll("[data-select-key]").forEach((element) => {
    element.addEventListener("change", (event) => {
      state.selectedKeyId = event.target.value;
      renderPage();
    });
  });

  document.querySelectorAll("[data-auto-repair]").forEach((element) => {
    element.addEventListener("change", (event) => {
      state.repair.auto = event.target.checked;
      writeBooleanSetting("2xapi.autoPreviewHistory", state.repair.auto);
      toast(state.repair.auto ? "已启用启动时自动预览。" : "已关闭启动时自动预览。", "warning");
    });
  });

  document.querySelectorAll("[data-retention-days]").forEach((element) => {
    element.addEventListener("change", (event) => {
      const value = Math.min(3650, Math.max(1, Number.parseInt(event.target.value, 10) || 30));
      state.retentionDays = value;
      writeNumberSetting("2xapi.backupRetentionDays", value);
      event.target.value = String(value);
    });
  });

  document.querySelectorAll("[data-action]").forEach((element) => {
    element.addEventListener("click", () => {
      const action = element.dataset.action;
      if (action === "open-external") { const href = element.dataset.href; if (href) window.open(href, "_blank", "noopener"); return; }
      if (action === "open-login") { state.loginModalOpen = true; state.authError = null; renderPage(); return; }
      if (action === "close-login-modal") { state.loginModalOpen = false; state.authError = null; state.twoFactorRequired = false; renderPage(); return; }
      if (action === "apply-platform") applyPlatform();
      if (action === "verify-captcha") {
        requestCaptchaProof();
        return;
      }
      if (action === "refresh-captcha") {
        hydrateCaptchaSettings();
        return;
      }
      if (action === "restore-official") restoreOfficial();
      if (action === "preview-repair") previewRepair();
      if (action === "repair") runRepair();
      if (action === "goto-xapi-keys") { state.view = "providers"; }
      if (action === "refresh-sessions") refreshSessions();
      if (action === "new-backup") createConfigSnapshot();
      if (action === "prune-backups") pruneBackups();
      if (action === "logout") logoutAccount();
      if (action === "refresh-token") refreshAuthorization();
      if (action === "apply-official") applyOfficialMode();
      if (action === "refresh-providers") refreshProviders();
      if (action === "import-provider-from-key") importProviderFromKey();
      if (action === "new-provider") openProviderFormNew();
      if (action === "close-provider-modal") closeProviderForm();
      if (action === "probe-provider") probeProviderForm();
      if (action === "reset-provider-form") resetProviderForm();
      if (!["refresh-sessions", "new-backup", "prune-backups", "logout", "refresh-token", "apply-official", "refresh-providers", "probe-provider", "import-provider-from-key"].includes(action)) renderPage();
    });
  });

  document.querySelectorAll("[data-restore-backup]").forEach((element) => {
    element.addEventListener("click", async () => {
      const backup = state.backups.find((item) => item.id === element.dataset.restoreBackup);
      if (backup?.path && window.codexApi) {
        try {
          const result = backup.type === "history" ? await window.codexApi.restoreHistory({ backupPath: backup.path }) : await window.codexApi.restoreConfig({ backupPath: backup.path });
          toast(result.written ? "备份已恢复，请重启 Codex。" : "已生成备份恢复预览，当前服务是 dry-run。", result.written ? "success" : "warning");
          if (result.written && backup.type !== "history") {
            state.currentMode = "official";
            state.lastConfigBackupPath = null;
          }
          renderPage();
        } catch (error) {
          toast(`恢复备份失败：${error.message}`, "warning");
        }
      } else {
        toast(`已选择恢复“${backup?.title || "备份"}”。`, "warning");
      }
    });
  });

  document.querySelectorAll("[data-provider-form]").forEach((form) => {
    form.addEventListener("submit", (event) => { event.preventDefault(); handleProviderFormSubmit(form); });
  });
  document.querySelectorAll("[data-apply-provider]").forEach((element) => {
    element.addEventListener("click", () => applyCustomProvider(element.dataset.applyProvider));
  });
  document.querySelectorAll("[data-edit-provider]").forEach((element) => {
    element.addEventListener("click", () => editProvider(element.dataset.editProvider));
  });
  document.querySelectorAll("[data-delete-provider]").forEach((element) => {
    element.addEventListener("click", () => deleteCustomProvider(element.dataset.deleteProvider));
  });
}

async function logoutAccount() {
  try {
    if (window.codexApi) await window.codexApi.logout();
  } catch (error) {
    toast(`退出登录失败：${error.message}`, "warning");
    return;
  }
  state.authenticated = false;
  state.currentMode = "official";
  state.view = "providers";
  toast("2xapi 账号已退出；官方 Codex 登录保持不变。", "warning");
  renderPage();
}

async function hydrateCaptchaSettings() {
  if (!window.codexApi) return false;
  state.captcha.loading = true;
  try {
    const settings = await window.codexApi.captcha();
    state.captcha = { ...state.captcha, ...settings, proof: null, loading: false, error: null };
    return true;
  } catch {
    state.captcha = { ...state.captcha, enabled: false, provider: null, proof: null, loading: false, error: "无法读取登录安全配置，请检查网络连接。" };
    return false;
  } finally {
    renderPage();
  }
}

async function hydrateApiData() {
  if (!window.codexApi) {
    state.demoMode = true;
    state.authenticated = true;
    renderPage();
    return;
  }
  try {
    // --- Local data (no 2xapi auth required) ---
    state.health = await window.codexApi.health();
    state.currentMode = state.health.provider?.providerId && state.health.provider.providerId !== "openai" ? "platform" : "official";
    await hydrateCaptchaSettings();
    try { state.repair.inspection = await window.codexApi.inspectHistory(); } catch (error) { state.repair.result = `历史会话尚未扫描：${error.message}`; }
    try {
      const backupPayload = await window.codexApi.backups();
      if (Array.isArray(backupPayload.backups)) {
        state.backups = backupPayload.backups.map((backup) => ({
          id: backup.id, type: backup.kind, purpose: backup.purpose || null,
          title: backup.kind === "history" ? "历史会话修复前" : backup.purpose === "manual" ? "手动配置快照" : "应用配置前",
          kind: backup.kind === "history" ? "会话备份" : "配置备份",
          path: backup.path, date: new Date(backup.createdAt).toLocaleString("zh-CN"),
        }));
        state.lastConfigBackupPath = state.backups.find((backup) => backup.type === "config" && backup.purpose === "pre-apply")?.path || state.lastConfigBackupPath;
      }
    } catch { /* backups optional */ }
    try {
      const providerPayload = await window.codexApi.listProviders();
      state.providers = providerPayload.providers || [];
      state.currentProviderId = providerPayload.currentProviderId || null;
    } catch { /* providers optional */ }
    renderPage();

    // --- 2xapi auth (background; doesn't block other pages) ---
    let session;
    try { session = await window.codexApi.session(); } catch { session = null; }
    if (session) {
      state.authenticated = session.authenticated !== false;
      if (session.user) { state.account.email = session.user.email || state.account.email; state.account.name = session.user.name || state.account.name; state.account.tenant = session.user.tenant || state.account.tenant; }
    }
    if (!state.authenticated && !state.autoLoginAttempted) {
      state.autoLoginAttempted = true;
      try {
        const r = await window.codexApi.remembered();
        if (r.remembered) { state.rememberedEmail = r.email || ""; state.rememberedPassword = r.password || ""; state.rememberLogin = true; }
        if (r.remembered && r.email && r.password && !state.captcha.enabled) {
          try {
            const auto = await window.codexApi.login({ email: r.email, password: r.password });
            if (auto.authenticated) {
              state.authenticated = true;
              if (auto.user) { state.account.email = auto.user.email || state.account.email; state.account.name = auto.user.name || state.account.name; state.account.tenant = auto.user.tenant || state.account.tenant; }
              toast("已用记住的账号自动登录。");
            }
          } catch { /* 自动登录失败（密码已改/网络），预填表单让用户手动登 */ }
        }
      } catch { /* 读取记住凭据失败忽略 */ }
    }
    if (state.authenticated) {
      try {
        const payload = await window.codexApi.loadKeyGroups();
        if (Array.isArray(payload.groups) && payload.groups.length > 0) groups = payload.groups.map((group) => ({
          ...group, note: `${group.keys.filter((key) => key.status === "active").length} 个可用 Key`,
          keys: group.keys.map((key) => ({ ...key, masked: key.masked || key.maskedValue, created: key.created || key.createdAt ? new Date(key.created || key.createdAt).toLocaleDateString("zh-CN") : "待同步", quota: key.quota, quotaUsed: key.quotaUsed })),
        }));
        if (groups.length > 0) { const initialGroup = firstUsableGroup(groups) || groups[0]; state.selectedGroupId = initialGroup.id; state.selectedKeyId = resolveSelectedKey(groups, initialGroup.id, "")?.id || ""; }
      } catch { /* key groups optional */ }
    }
    renderPage();
    if (state.repair.auto && state.authenticated) await previewRepair();
  } catch (error) {
    if (error.status === 404 || error.code === "NETWORK_ERROR") {
      state.demoMode = true;
      state.authenticated = true;
      toast("本地服务未连接，当前显示演示数据。", "warning");
    } else { toast(`接口暂不可用：${error.message}`, "warning"); }
    renderPage();
  }
}

renderPage();
hydrateApiData();

window.keySelection = { firstUsableGroup, resolveSelectedGroup, resolveSelectedKey };
