// Small behaviours on top of the server-rendered dashboard.

// Theme toggle: icon and label follow `data-theme` in CSS; only the
// accessible name needs updating here.
function syncThemeToggle() {
  const next = window.entitleTheme.current() === "dark" ? "light" : "dark";
  for (const button of document.querySelectorAll("[data-theme-toggle]")) {
    button.setAttribute("aria-label", `Switch to ${next} theme`);
  }
}

document.addEventListener("click", (e) => {
  const toggle = e.target.closest("[data-theme-toggle]");
  if (toggle) window.entitleTheme.set(window.entitleTheme.current() === "dark" ? "light" : "dark");
});

// <select data-navigate>: each option's value is the URL to open.
// <input type="checkbox" data-navigate="URL">: toggling opens URL.
document.addEventListener("change", (e) => {
  const input = e.target.closest("[data-navigate]");
  if (!input) return;
  location.assign(input instanceof HTMLSelectElement ? input.value : input.dataset.navigate);
});

addEventListener("entitle:theme", syncThemeToggle);
document.addEventListener("DOMContentLoaded", syncThemeToggle);

// <time data-local="…" datetime="ISO">: the server writes UTC; show the
// viewer's time zone with the options the page used before.
const LOCAL_FORMATS = {
  activity: { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit", second: "2-digit" },
  customer: { day: "numeric", month: "short", year: "numeric", hour: "2-digit", minute: "2-digit" },
  date: { day: "numeric", month: "short", year: "numeric" },
};

function localTimes(root = document) {
  for (const t of root.querySelectorAll("time[data-local]")) {
    const format = LOCAL_FORMATS[t.dataset.local];
    const date = new Date(t.dateTime);
    t.textContent = format.hour ? date.toLocaleString("en", format) : date.toLocaleDateString("en", format);
  }
}

document.addEventListener("DOMContentLoaded", () => localTimes());

// <section data-refresh="URL">: replaced by a fresh render of URL on
// [data-refresh-now] clicks inside it, and every 10 s (while the tab is
// visible) with data-refresh-auto.
async function refresh(section) {
  const res = await fetch(section.dataset.refresh, { headers: { accept: "text/html" } });
  if (!res.ok) return;
  const next = new DOMParser().parseFromString(await res.text(), "text/html").body.firstElementChild;
  if (!next) return;
  localTimes(next);
  section.replaceWith(next);
}

document.addEventListener("click", (e) => {
  const button = e.target.closest("[data-refresh-now]");
  const section = button?.closest("[data-refresh]");
  if (section) refresh(section);
});

document.addEventListener("DOMContentLoaded", () => {
  setInterval(() => {
    if (document.hidden) return;
    for (const section of document.querySelectorAll("[data-refresh][data-refresh-auto]")) refresh(section);
  }, 10_000);
});

// [data-copy="value"]: copies the value, then shows its check mark for a moment.
document.addEventListener("click", async (e) => {
  const button = e.target.closest("[data-copy]");
  if (!button) return;
  try {
    await navigator.clipboard.writeText(button.dataset.copy);
    button.dataset.copied = "";
    setTimeout(() => delete button.dataset.copied, 1500);
  } catch {
    // Clipboard blocked: the value stays selectable.
  }
});

// Destructive buttons ask twice: [data-confirm-ask] reveals the sibling
// .confirm (question, the real submit button, [data-confirm-cancel]).
document.addEventListener("click", (e) => {
  const ask = e.target.closest("[data-confirm-ask]");
  const cancel = e.target.closest("[data-confirm-cancel]");
  const wrap = (ask ?? cancel)?.closest("[data-confirm]");
  if (!wrap) return;
  wrap.querySelector("[data-confirm-ask]").hidden = !!ask;
  wrap.querySelector(".confirm").hidden = !ask;
});

// ---- setup forms ----

// <input type="file" data-read-into="field" data-expect="p8|service_account">:
// the file is read here and posted as text in the named field, as before;
// it never leaves the browser except to this server.
document.addEventListener("change", async (e) => {
  const input = e.target.closest("input[type=file][data-read-into]");
  if (!input) return;
  const form = input.form;
  const target = form.elements[input.dataset.readInto];
  const error = form.querySelector("[data-file-error]");
  target.value = "";
  error.hidden = true;
  const file = input.files?.[0];
  if (!file) return;
  const text = await file.text();
  let ok;
  if (input.dataset.expect === "p8") {
    ok = text.includes("PRIVATE KEY");
    // App Store Connect names the file after the key id: SubscriptionKey_ABC123.p8.
    const id = /_([A-Z0-9]+)\.p8$/i.exec(file.name)?.[1];
    const keyId = form.elements.key_id;
    if (ok && id && keyId && !keyId.value) keyId.value = id;
  } else {
    try {
      ok = JSON.parse(text).type === "service_account";
    } catch {
      ok = false;
    }
  }
  if (ok) {
    target.value = text;
  } else {
    input.value = "";
    error.hidden = false;
  }
});

// [data-edit]: [data-edit-open] shows its [data-edit-form] and hides the
// [data-edit-idle] controls; [data-edit-cancel] goes back.
document.addEventListener("click", (e) => {
  const open = e.target.closest("[data-edit-open]");
  const cancel = e.target.closest("[data-edit-cancel]");
  const wrap = (open ?? cancel)?.closest("[data-edit]");
  if (!wrap) return;
  for (const el of wrap.querySelectorAll("[data-edit-idle]")) el.hidden = !!open;
  wrap.querySelector("[data-edit-form]").hidden = !open;
});

// <select data-autosubmit>: saves on change.
document.addEventListener("change", (e) => {
  const select = e.target.closest("select[data-autosubmit]");
  if (select) select.form.requestSubmit();
});

// <input data-reveal-when="text" data-reveals="id">: shows #id only while the
// input holds exactly that text (e.g. the project name before deleting it).
document.addEventListener("input", (e) => {
  const input = e.target.closest("[data-reveal-when]");
  if (input) document.getElementById(input.dataset.reveals).hidden = input.value !== input.dataset.revealWhen;
});

// <form data-dirty>: its submit button stays disabled until a value changes.
function formState(form) {
  return JSON.stringify([...new FormData(form)].map(([k, v]) => [k, typeof v === "string" ? v.trim() : ""]));
}
document.addEventListener("DOMContentLoaded", () => {
  for (const form of document.querySelectorAll("form[data-dirty]")) {
    const initial = formState(form);
    const submit = form.querySelector("[type=submit]");
    submit.disabled = true;
    form.addEventListener("input", () => (submit.disabled = formState(form) === initial));
  }
});

// <button data-href="URL">: a button that opens URL (Refresh, Cancel).
document.addEventListener("click", (e) => {
  const button = e.target.closest("button[data-href]");
  if (button) location.assign(button.dataset.href);
});

// [data-dismiss] removes its [data-dismissable] container (e.g. a shown secret).
document.addEventListener("click", (e) => {
  e.target.closest("[data-dismiss]")?.closest("[data-dismissable]")?.remove();
});
