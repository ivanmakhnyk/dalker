// Dalker translations: picks the language, loads its strings from i18n/<code>.json, translates
// the page's static text and then starts the page's own script (`data-main` on this script's
// tag), so that script can use I18N.t() from its first line.
//
// The language is the one the person chose, or else the first supported one of:
// * Dalker Settings on the host (the URL has the per-launch key): the host's operating system
//   languages, as the settings API reports them. The choice is saved in the config file.
// * the web client (and its "AI agents" panel): the browser's languages. The choice is kept in
//   this browser.
// English is the fallback, for the language and for each missing string.
//
// Static text in the HTML is marked with data-i18n="key" (text), data-i18n-title,
// data-i18n-aria-label and data-i18n-placeholder. A string with placeholders is written
// "{name}"; a string that depends on a count is an object of plural forms ("one", "few",
// "many", "other", as Intl.PluralRules names them) and takes the count as {n}.
"use strict";

(() => {
  const STORE_KEY = "dalker.language";
  const script = document.currentScript;
  const hostKey = new URLSearchParams(location.hash.slice(1)).get("key");
  const listeners = [];
  let languages = [{ code: "en", name: "English" }];
  let english = {};
  let strings = {};
  let lang = "en";
  let choice = "auto";
  let system = [];
  let plurals = new Intl.PluralRules("en");

  // ------------------------------------------------------------ choosing the language

  // The supported language for one language tag (BCP 47 like "uk-UA", or a POSIX locale like
  // "pt_BR.UTF-8"), or null. Same rules as `resolve` in dalker-platform's i18n.rs.
  function matchTag(tag) {
    const clean = String(tag || "").split(/[.@]/)[0].replace(/_/g, "-").toLowerCase();
    if (!clean || clean === "c" || clean === "posix") return null;
    const parts = clean.split("-");
    const base = parts[0];
    const region = parts.slice(1).find((p) => /^[a-z]{2}$|^\d{3}$/.test(p));
    const codes = languages.map((l) => l.code);
    const has = (c) => codes.includes(c);
    let pick = null;
    if (base === "zh") pick = parts.includes("hant") || ["tw", "hk", "mo"].includes(region) ? "zh-TW" : "zh-CN";
    else if (base === "pt") pick = !region || region === "br" ? "pt-BR" : "pt-PT";
    else if (base === "no" || base === "nn" || base === "nb") pick = "nb";
    else if (base === "iw") pick = "he";
    else if (base === "in") pick = "id";
    if (pick && has(pick)) return pick;
    return codes.find((c) => c.toLowerCase() === clean) || codes.find((c) => c.split("-")[0].toLowerCase() === base) || null;
  }

  function resolve() {
    if (choice !== "auto" && languages.some((l) => l.code === choice)) return choice;
    const prefs = hostKey ? system : (navigator.languages && navigator.languages.length ? navigator.languages : [navigator.language]);
    for (const tag of prefs) {
      const code = matchTag(tag);
      if (code) return code;
    }
    return "en";
  }

  // ------------------------------------------------------------ strings

  const own = (o, key) => Object.prototype.hasOwnProperty.call(o, key);
  const known = (key) => own(strings, key) || own(english, key);

  function lookup(key) {
    if (own(strings, key)) return strings[key];
    if (own(english, key)) return english[key];
    return key;
  }

  // The string `key` with its {placeholders} filled in from `params`.
  function t(key, params) {
    let s = lookup(key);
    if (s && typeof s === "object") {
      const n = params && typeof params.n === "number" ? params.n : 0;
      s = s[plurals.select(n)] ?? s.other ?? "";
    }
    s = String(s);
    if (!params) return s;
    return s.replace(/\{(\w+)\}/g, (m, name) => (params[name] === undefined ? m : String(params[name])));
  }

  // A number written the way the language writes it.
  function number(n, maxDigits = 0, minDigits = 0) {
    return new Intl.NumberFormat(lang, { maximumFractionDigits: maxDigits, minimumFractionDigits: minDigits }).format(n);
  }

  const ATTRS = [["i18nTitle", "title"], ["i18nAriaLabel", "aria-label"], ["i18nPlaceholder", "placeholder"]];

  // Translates the marked static text under `root`. Text without a known string stays as the
  // HTML has it (in English).
  function apply(root = document) {
    for (const el of root.querySelectorAll("[data-i18n]")) {
      if (known(el.dataset.i18n)) el.textContent = t(el.dataset.i18n);
    }
    for (const [data, attr] of ATTRS) {
      const sel = `[data-${data.replace(/[A-Z]/g, (c) => "-" + c.toLowerCase())}]`;
      for (const el of root.querySelectorAll(sel)) {
        if (known(el.dataset[data])) el.setAttribute(attr, t(el.dataset[data]));
      }
    }
    const dir = (languages.find((l) => l.code === lang) || {}).dir || "ltr";
    document.documentElement.lang = lang;
    document.documentElement.dir = dir;
  }

  // ------------------------------------------------------------ loading

  async function json(path, init) {
    const r = await fetch(path, { credentials: "same-origin", cache: "no-cache", ...init });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    return r.json();
  }

  async function load(code) {
    lang = code;
    plurals = new Intl.PluralRules(code);
    strings = code === "en" ? english : await json(`i18n/${code}.json`).catch(() => ({}));
  }

  function hostLanguage(body) {
    const headers = { "X-Dalker-Key": hostKey };
    if (body) headers["Content-Type"] = "application/json";
    return json("api/settings/language", body ? { method: "POST", headers, body: JSON.stringify(body) } : { headers });
  }

  // Changes the language: a code from the list, or "auto" to follow the system or browser.
  async function setChoice(next) {
    if (hostKey) {
      const r = await hostLanguage({ language: next });
      choice = r.choice || "auto";
      system = r.system || system;
    } else {
      choice = next;
      try {
        if (next === "auto") localStorage.removeItem(STORE_KEY);
        else localStorage.setItem(STORE_KEY, next);
      } catch (_) { /* storage blocked: the choice lasts until the page is closed */ }
    }
    await load(resolve());
    apply();
    for (const f of listeners) f(lang);
  }

  window.I18N = {
    t,
    number,
    apply,
    setChoice,
    language: () => lang,
    choice: () => choice,
    languages: () => languages.slice(),
    // Called after every change of language, with the new code.
    onChange: (f) => listeners.push(f),
  };

  (async () => {
    const [list, en, host] = await Promise.all([
      json("i18n/languages.json").catch(() => null),
      json("i18n/en.json").catch(() => ({})),
      hostKey ? hostLanguage().catch(() => null) : null,
    ]);
    if (Array.isArray(list) && list.length) languages = list;
    english = en;
    if (host) {
      choice = host.choice || "auto";
      system = host.system || [];
    } else if (!hostKey) {
      try { choice = localStorage.getItem(STORE_KEY) || "auto"; } catch (_) { choice = "auto"; }
    }
    await load(resolve());
    apply();
  })().catch(() => {}).finally(() => {
    // The page's own script starts in any case, in English if nothing could be loaded.
    const main = document.createElement("script");
    main.src = script.dataset.main;
    document.body.appendChild(main);
  });
})();
