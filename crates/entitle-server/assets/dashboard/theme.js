// Light/dark theme: follows the OS until the viewer picks one (remembered per
// browser). Loaded in <head>, before the body renders, so there is no flash.
(() => {
  const KEY = "entitle.theme";
  const media = matchMedia("(prefers-color-scheme: dark)");
  const stored = () => {
    try {
      const v = localStorage.getItem(KEY);
      return v === "light" || v === "dark" ? v : null;
    } catch {
      return null;
    }
  };
  const current = () => stored() ?? (media.matches ? "dark" : "light");
  const apply = () => {
    document.documentElement.dataset.theme = current();
    dispatchEvent(new Event("entitle:theme"));
  };
  window.entitleTheme = {
    current,
    set(theme) {
      try {
        localStorage.setItem(KEY, theme);
      } catch {
        // Storage blocked: the choice lasts for this page only.
      }
      apply();
    },
  };
  media.addEventListener("change", apply);
  apply();
})();
