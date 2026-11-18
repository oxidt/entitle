// uPlot charts for the server-rendered revenue page: categorical x (one slot
// per bucket), stacked bars drawn on the canvas (positives up from zero,
// negatives down), plain 2px lines and a shared crosshair tooltip.
//
// Each `<div class="chart" data-chart="…">` carries its config as JSON:
//   { labels, height, partialFrom, axis: { kind: "money" | "count" | "pct", currency },
//     series: [{ label, color, kind: "bar" | "line", values, compare?, markers?, key? }],
//     tips: [{ title, rows: [{ label, value, color?, line?, strong?, key? }] }] }
// Series and tooltip rows with a `key` follow the card's legend toggles
// (`button[data-legend-key]`); the hidden keys are kept for the browser tab.

(() => {
  const INK_MUTED = "var(--chart-ink)";
  const GRID = "var(--chart-grid)";
  const BASELINE = "var(--chart-baseline)";
  const SURFACE = "var(--surface)";
  const HIDDEN_KEY = "entitle.hidden";

  /** Resolves `var(--x)` to its current value; canvas drawing needs real colors. */
  function cssColor(color) {
    const m = /^var\((--[\w-]+)\)$/.exec(color);
    return m ? getComputedStyle(document.documentElement).getPropertyValue(m[1]).trim() || color : color;
  }

  // ---- number formatting for axes ----

  const fmtCache = new Map();
  function nf(key, make) {
    let f = fmtCache.get(key);
    if (!f) {
      f = make();
      fmtCache.set(key, f);
    }
    return f;
  }
  const minorExponent = (currency) =>
    nf(`exp:${currency}`, () => new Intl.NumberFormat("en", { style: "currency", currency })).resolvedOptions()
      .maximumFractionDigits ?? 2;
  const toMajor = (minor, currency) => minor / 10 ** minorExponent(currency);
  const moneyCompact = (major, currency) =>
    nf(
      `mc:${currency}`,
      () => new Intl.NumberFormat("en", { style: "currency", currency, notation: "compact", maximumSignificantDigits: 3 }),
    ).format(major);
  const intCompact = (n) =>
    nf("ic", () => new Intl.NumberFormat("en", { notation: "compact", maximumSignificantDigits: 3 })).format(n);
  const percent = (ratio, digits) => `${(ratio * 100).toFixed(digits)}%`;

  /** Axis label formatter; minor units and counts are whole numbers, so fractional ticks stay unlabelled. */
  function axisFormat(axis) {
    if (axis.kind === "money") return (x) => (Number.isInteger(x) ? moneyCompact(toMajor(x, axis.currency), axis.currency) : "");
    if (axis.kind === "count") return (x) => (Number.isInteger(x) ? intCompact(x) : "");
    return (x) => percent(x, x !== 0 && Math.abs(x) < 0.1 ? 1 : 0);
  }

  // ---- drawing ----

  function niceRange(min, max) {
    if (min === 0 && max === 0) return [0, 1];
    if (min === max) return [min - 1, max + 1];
    const r = uPlot.rangeNum(min, max, 0.08, true);
    return [Math.min(0, r[0] ?? 0), Math.max(0, r[1] ?? 0)];
  }

  function stackExtent(series) {
    let min = 0;
    let max = 0;
    const n = series[0]?.values.length ?? 0;
    for (let i = 0; i < n; i++) {
      let pos = 0;
      let neg = 0;
      for (const s of series) {
        const v = s.values[i] ?? 0;
        if (s.kind === "bar") {
          if (v >= 0) pos += v;
          else neg += v;
        } else {
          max = Math.max(max, v);
          min = Math.min(min, v);
        }
      }
      max = Math.max(max, pos);
      min = Math.min(min, neg);
    }
    return [min, max];
  }

  function drawBars(u, series) {
    const ctx = u.ctx;
    const dpr = devicePixelRatio || 1;
    const n = u.data[0].length;
    const slot = u.bbox.width / Math.max(n, 1);
    const w = Math.max(2 * dpr, Math.min(slot * 0.62, 24 * dpr));
    const gap = 2 * dpr;
    const radius = 4 * dpr;
    const y0 = Math.round(u.valToPos(0, "y", true));
    ctx.save();
    ctx.beginPath();
    ctx.rect(u.bbox.left, u.bbox.top, u.bbox.width, u.bbox.height);
    ctx.clip();
    const bars = series.map((s, k) => ({ s, k: k + 1 })).filter(({ s, k }) => s.kind === "bar" && u.series[k]?.show !== false);
    for (let i = 0; i < n; i++) {
      const cx = u.valToPos(i, "x", true);
      for (const sign of [1, -1]) {
        const segs = bars.filter(({ s }) => Math.sign(s.values[i] ?? 0) === sign);
        let acc = 0;
        segs.forEach(({ s }, j) => {
          const v = s.values[i] ?? 0;
          const from = Math.round(u.valToPos(acc, "y", true));
          acc += v;
          const to = Math.round(u.valToPos(acc, "y", true));
          // 2px surface gap between stacked segments, none at the baseline.
          let top = Math.min(from, to);
          let bottom = Math.max(from, to);
          if (j > 0) {
            if (sign > 0) bottom -= gap;
            else top += gap;
          }
          const h = bottom - top;
          if (h <= 0) return;
          const last = j === segs.length - 1;
          const r = last ? Math.min(radius, h) : 0;
          ctx.fillStyle = cssColor(s.color);
          ctx.beginPath();
          ctx.roundRect(cx - w / 2, top, w, h, sign > 0 ? [r, r, 0, 0] : [0, 0, r, r]);
          ctx.fill();
        });
      }
    }
    ctx.strokeStyle = cssColor(BASELINE);
    ctx.lineWidth = dpr;
    ctx.beginPath();
    ctx.moveTo(u.bbox.left, y0 + 0.5);
    ctx.lineTo(u.bbox.left + u.bbox.width, y0 + 0.5);
    ctx.stroke();
    ctx.restore();
  }

  /** Fades the in-progress buckets (bars and lines) behind a dashed boundary. */
  function shadePartial(u, from) {
    const ctx = u.ctx;
    const dpr = devicePixelRatio || 1;
    const x = Math.round(u.valToPos(from - 0.5, "x", true));
    const right = u.bbox.left + u.bbox.width;
    if (x >= right) return;
    ctx.save();
    ctx.globalAlpha = 0.6;
    ctx.fillStyle = cssColor(SURFACE);
    ctx.fillRect(x, u.bbox.top, right - x, u.bbox.height);
    ctx.globalAlpha = 1;
    ctx.strokeStyle = cssColor(BASELINE);
    ctx.lineWidth = dpr;
    ctx.setLineDash([3 * dpr, 3 * dpr]);
    ctx.beginPath();
    ctx.moveTo(x + 0.5, u.bbox.top);
    ctx.lineTo(x + 0.5, u.bbox.top + u.bbox.height);
    ctx.stroke();
    ctx.restore();
  }

  // ---- tooltip ----

  function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  }

  function key(row) {
    const k = el("span", row.color ? (row.line ? "key line" : "key") : "key");
    if (row.color) k.style.background = row.color;
    return k;
  }

  function showTip(root, tip, left, hidden) {
    root.querySelector(".tip")?.remove();
    if (!tip) return;
    const width = root.clientWidth;
    const box = el("div", "tip");
    if (left > width / 2) box.style.right = `${width - left + 12}px`;
    else box.style.left = `${left + 12}px`;
    box.append(el("div", "tip-title", tip.title));
    for (const r of tip.rows) {
      if (r.key && hidden.has(r.key)) continue;
      const row = el("div", `tip-row${r.strong ? " strong" : ""}`);
      row.append(key(r), el("span", "tip-value", r.value), el("span", "tip-label", r.label));
      box.append(row);
    }
    root.prepend(box);
  }

  // ---- charts ----

  function loadHidden() {
    try {
      return new Set(JSON.parse(sessionStorage.getItem(HIDDEN_KEY) ?? "[]"));
    } catch {
      return new Set();
    }
  }

  function saveHidden(hidden) {
    try {
      sessionStorage.setItem(HIDDEN_KEY, JSON.stringify([...hidden]));
    } catch {
      // Storage blocked: toggles last for this page only.
    }
  }

  const hidden = loadHidden();
  const charts = [];

  function mount(root) {
    const config = JSON.parse(root.dataset.chart);
    const chart = { root, config, plot: null, ro: null };
    chart.draw = () => draw(chart);
    root.addEventListener("mouseleave", () => showTip(root, null));
    charts.push(chart);
    chart.draw();
  }

  function draw(chart) {
    const { root, config } = chart;
    chart.ro?.disconnect();
    chart.plot?.destroy();
    showTip(root, null);
    const { labels, height, partialFrom } = config;
    const series = config.series.filter((s) => !(s.key && hidden.has(s.key)));
    const n = labels.length;
    const [lo, hi] = stackExtent(series);
    const font = "11px system-ui, -apple-system, 'Segoe UI', sans-serif";
    const hasBars = series.some((s) => s.kind === "bar");
    const axis = axisFormat(config.axis);
    const opts = {
      width: root.clientWidth,
      height,
      legend: { show: false },
      padding: [8, 20, 0, 0],
      cursor: {
        y: false,
        drag: { x: false, y: false },
        points: { show: !hasBars, size: 8, width: 2, stroke: cssColor(SURFACE) },
      },
      scales: {
        x: { time: false, range: () => [-0.5, n - 0.5] },
        y: { range: () => niceRange(lo, hi) },
      },
      axes: [
        {
          stroke: cssColor(INK_MUTED),
          font,
          grid: { show: false },
          ticks: { show: false },
          gap: 6,
          splits: (u) => {
            const step = Math.max(1, Math.ceil(n / (u.bbox.width / (devicePixelRatio || 1) / 64)));
            return labels.map((_, i) => i).filter((i) => (n - 1 - i) % step === 0);
          },
          values: (_u, splits) => splits.map((i) => labels[i] ?? ""),
        },
        {
          stroke: cssColor(INK_MUTED),
          font,
          size: 56,
          gap: 6,
          // Short charts would otherwise get only two labelled ticks.
          space: Math.min(30, height / 6),
          grid: { stroke: cssColor(GRID), width: 1 },
          ticks: { show: false },
          values: (_u, splits) => splits.map(axis),
        },
      ],
      series: [
        {},
        ...series.map((s) =>
          s.kind === "bar"
            ? { label: s.label, paths: () => null, points: { show: false } }
            : {
                label: s.label,
                stroke: cssColor(s.color),
                width: s.compare ? 1.5 : 2,
                points: { show: n <= 1 || !!s.markers, size: 8, fill: cssColor(s.color), stroke: cssColor(SURFACE), width: 2 },
              },
        ),
      ],
      hooks: {
        drawAxes: [(u) => drawBars(u, series)],
        draw: partialFrom != null && partialFrom >= 0 ? [(u) => shadePartial(u, partialFrom)] : [],
        setCursor: [
          (u) => {
            const i = u.cursor.idx;
            const off = i == null || u.cursor.left == null || u.cursor.left < 0;
            showTip(root, off ? null : config.tips[i], off ? 0 : u.over.offsetLeft + u.valToPos(i, "x"), hidden);
          },
        ],
      },
    };
    const data = [labels.map((_, i) => i), ...series.map((s) => s.values)];
    chart.plot = new uPlot(opts, data, root);
    chart.ro = new ResizeObserver(() => chart.plot.setSize({ width: root.clientWidth, height }));
    chart.ro.observe(root);
  }

  /** Legend toggles: the button's pressed state and the keyed legend items follow `hidden`. */
  function syncLegends() {
    for (const b of document.querySelectorAll("button[data-legend-key]")) {
      b.setAttribute("aria-pressed", String(!hidden.has(b.dataset.legendKey)));
    }
    for (const item of document.querySelectorAll("[data-legend-follows]")) {
      item.hidden = hidden.has(item.dataset.legendFollows);
    }
  }

  document.addEventListener("click", (e) => {
    const b = e.target.closest("button[data-legend-key]");
    if (!b) return;
    const k = b.dataset.legendKey;
    if (!hidden.delete(k)) hidden.add(k);
    saveHidden(hidden);
    syncLegends();
    for (const c of charts) if (c.root.closest(".card") === b.closest(".card")) c.draw();
  });

  addEventListener("entitle:theme", () => charts.forEach((c) => c.draw()));

  function init() {
    syncLegends();
    document.querySelectorAll(".chart[data-chart]").forEach(mount);
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
})();
