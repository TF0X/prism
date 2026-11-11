// Injected into every page at document-start. Makes the page translucent so
// the window's acrylic backdrop reads through it, without touching text.
//
// The model: one translucent layer on <html>, every full-bleed surface above
// it cleared outright. Bare regions then match tinted ones instead of showing
// raw acrylic as a black bar. backdrop-filter has to be neutralised too --
// with the background cleared there is nothing opaque left for a frosted
// header to sample, so it composites against transparent black.
(function () {
  "use strict";

  var LEVELS = [
    { alpha: 1,    name: "Off" },
    { alpha: 0.45, name: "Frosted" },
    { alpha: 0.15, name: "Clear" }
  ];
  var SKIP = { SCRIPT: 1, STYLE: 1, IMG: 1, VIDEO: 1, CANVAS: 1, SVG: 1, TEXTAREA: 1, INPUT: 1 };
  var STYLE_ID = "__prism_glass";
  var PROPS = ["background-color", "background-image", "backdrop-filter", "-webkit-backdrop-filter"];

  var level = 0;
  var touched = [];
  var base = null;
  var baseLocked = false;
  var timer = null;

  function send(action, extra) {
    var m = { action: action };
    if (extra) for (var k in extra) m[k] = extra[k];
    try { window.ipc.postMessage(JSON.stringify(m)); } catch (e) {}
  }

  function captureBase() {
    if (baseLocked) return;
    var cands = [document.body, document.documentElement];
    for (var i = 0; i < cands.length; i++) {
      if (!cands[i]) continue;
      var m = /^rgba?\((\d+),\s*(\d+),\s*(\d+)(?:,\s*([\d.]+))?\)/.exec(
        getComputedStyle(cands[i]).backgroundColor
      );
      if (m && (m[4] === undefined || parseFloat(m[4]) > 0.9)) {
        base = m[1] + "," + m[2] + "," + m[3];
        baseLocked = true;
        return;
      }
    }
    // Provisional: sampling can precede the site's CSS, so allow a later upgrade.
    base = base || "20,20,26";
  }

  function keep(e) {
    var saved = [e];
    for (var i = 0; i < PROPS.length; i++) {
      saved.push(e.style.getPropertyValue(PROPS[i]));
      saved.push(e.style.getPropertyPriority(PROPS[i]));
    }
    touched.push(saved);
  }

  function restore() {
    for (var i = 0; i < touched.length; i++) {
      var t = touched[i];
      for (var p = 0; p < PROPS.length; p++) {
        var val = t[1 + p * 2];
        if (val) t[0].style.setProperty(PROPS[p], val, t[2 + p * 2]);
        else t[0].style.removeProperty(PROPS[p]);
      }
    }
    touched = [];
  }

  // Breadth-first and node-capped, so a huge page cannot turn this into a
  // layout-thrashing crawl.
  function clearSurfaces(root) {
    var queue = [[root, 0]];
    var seen = 0;
    while (queue.length && seen < 800) {
      var item = queue.shift();
      if (item[1] > 8 || !item[0].children) continue;

      for (var i = 0; i < item[0].children.length; i++) {
        var e = item[0].children[i];
        if (SKIP[e.tagName]) continue;
        seen++;

        var r = e.getBoundingClientRect();
        if (r.width >= innerWidth * 0.6) {
          var cs = getComputedStyle(e);
          var full = r.height >= innerHeight * 0.4;
          // Headers, toolbars and composer bars: full width but short.
          var bar =
            !full && r.height >= 24 &&
            (cs.position === "fixed" || cs.position === "sticky" ||
             r.top <= 2 || r.bottom >= innerHeight - 2);

          if (full || bar) {
            var m = /^rgba?\((\d+),\s*(\d+),\s*(\d+)(?:,\s*([\d.]+))?\)/.exec(cs.backgroundColor);
            var opaque = m && (m[4] === undefined || parseFloat(m[4]) > 0.9);
            var img = cs.backgroundImage && cs.backgroundImage !== "none";
            var blur =
              (cs.backdropFilter && cs.backdropFilter !== "none") ||
              (cs.webkitBackdropFilter && cs.webkitBackdropFilter !== "none");

            if (opaque || img || blur) {
              keep(e);
              if (opaque) e.style.setProperty("background-color", "transparent", "important");
              if (img) e.style.setProperty("background-image", "none", "important");
              // Inline, because a site rule carrying !important on a class
              // outranks a universal stylesheet rule.
              if (blur) {
                e.style.setProperty("backdrop-filter", "none", "important");
                e.style.setProperty("-webkit-backdrop-filter", "none", "important");
              }
            }
          }
        }
        queue.push([e, item[1] + 1]);
      }
    }
  }

  function paint() {
    restore();
    var tag = document.getElementById(STYLE_ID);
    if (!level) {
      if (tag) tag.remove();
      return;
    }
    if (!tag) {
      tag = document.createElement("style");
      tag.id = STYLE_ID;
      (document.head || document.documentElement).appendChild(tag);
    }
    tag.textContent = "";          // clear ours so the sample sees real colours
    captureBase();
    tag.textContent =
      "html{background-color:rgba(" + base + "," + LEVELS[level].alpha + ")!important;" +
      "background-image:none!important}" +
      "body{background-color:transparent!important;background-image:none!important}" +
      "*,*::before,*::after{backdrop-filter:none!important;-webkit-backdrop-filter:none!important}";
    clearSurfaces(document.body || document.documentElement);
  }

  function settle() {
    clearTimeout(timer);
    timer = setTimeout(paint, 350);
  }

  window.__prismGlass = function (n) {
    level = Math.max(0, Math.min(LEVELS.length - 1, n | 0));
    paint();
  };

  function start() {
    if (document.body) {
      new MutationObserver(function () { if (level) settle(); })
        .observe(document.body, { childList: true, subtree: true });
    }
    addEventListener("resize", function () { if (level) settle(); }, { passive: true });
    send("ready");
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start, { once: true });
  } else {
    start();
  }
})();
