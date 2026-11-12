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

  // Kept light on purpose: the window's acrylic-blur backdrop already adds its
  // own tint, so a heavy page tint on top just smothers the blur. These only
  // need to lift text contrast a little over the frosted desktop.
  var LEVELS = [
    { alpha: 1,    name: "Off" },
    { alpha: 0.22, name: "Frosted" },
    { alpha: 0.06, name: "Clear" }
  ];
  var SKIP = { SCRIPT: 1, STYLE: 1, IMG: 1, VIDEO: 1, CANVAS: 1, SVG: 1, TEXTAREA: 1, INPUT: 1 };
  var STYLE_ID = "__prism_glass";
  var PROPS = ["background-color", "background-image", "backdrop-filter", "-webkit-backdrop-filter"];

  var level = 0;
  var touched = [];
  var base = null;
  var baseLocked = false;
  var timer = null;
  var chromeOffset = 0; // px the page is pushed down for the top chrome bar

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
    // Caps are generous: deep SPA shells like YouTube bury their filter bars
    // well below a shallow walk. Still bounded so a pathological page cannot
    // hang, and the whole sweep is debounced and runs rarely.
    var queue = [[root, 0]];
    var seen = 0;
    while (queue.length && seen < 2500) {
      var item = queue.shift();
      if (item[1] > 12 || !item[0].children) continue;

      for (var i = 0; i < item[0].children.length; i++) {
        var e = item[0].children[i];
        if (SKIP[e.tagName]) continue;
        seen++;

        var r = e.getBoundingClientRect();
        if (r.width >= innerWidth * 0.6) {
          var cs = getComputedStyle(e);
          var full = r.height >= innerHeight * 0.4;
          // Headers, toolbars, filter and composer bars: full width but short.
          // A short band is treated as chrome whatever its position -- relying on
          // fixed/sticky or edge-touching misses YouTube's absolute masthead,
          // and our own chrome-bar transform shifts page-top elements down past
          // any "touching the top" test, so compare against chromeOffset.
          var bar =
            !full && r.height >= 24 &&
            (r.height <= 108 ||
             cs.position === "fixed" || cs.position === "sticky" ||
             r.top <= chromeOffset + 4 || r.bottom >= innerHeight - 4);

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

  // Debug: walk up from a point and report the first element with an opaque or
  // image background, plus how it scores against each clearSurfaces test, so we
  // can see exactly why a bar was left solid. Fires only when Rust turns diag on.
  function probe(label, x, y) {
    var stack = document.elementsFromPoint(x, y) || [];
    for (var i = 0; i < stack.length; i++) {
      var e = stack[i];
      if (e.id === "__prism_grips" || e.tagName === "HTML" || e.tagName === "BODY") continue;
      var cs = getComputedStyle(e);
      var m = /^rgba?\((\d+),\s*(\d+),\s*(\d+)(?:,\s*([\d.]+))?\)/.exec(cs.backgroundColor);
      var opaque = m && (m[4] === undefined || parseFloat(m[4]) > 0.9);
      var img = cs.backgroundImage && cs.backgroundImage !== "none";
      if (!opaque && !img) continue;        // transparent layer, keep digging
      var r = e.getBoundingClientRect();
      var full = r.height >= innerHeight * 0.4;
      var wideEnough = r.width >= innerWidth * 0.6;
      var bar = !full && r.height >= 24 &&
        (r.height <= 108 || cs.position === "fixed" || cs.position === "sticky" ||
         r.top <= chromeOffset + 4 || r.bottom >= innerHeight - 4);
      send("diag", { msg:
        label + " -> " + e.tagName.toLowerCase() +
        "#" + (e.id || "") + "." + String(e.className).slice(0, 40) +
        " bg=" + cs.backgroundColor + (img ? " +img" : "") +
        " pos=" + cs.position +
        " " + Math.round(r.width) + "x" + Math.round(r.height) + "@y" + Math.round(r.top) +
        " | wide=" + wideEnough + " full=" + full + " bar=" + bar +
        " matched=" + (!!m) + " => " + ((wideEnough && (full || bar)) ? "SHOULD-CLEAR" : "SKIPPED")
      });
      return;
    }
    send("diag", { msg: label + " -> nothing opaque in stack" });
  }

  window.__prismProbe = function () {
    // Content is pushed down by the chrome bar (translateY), so a page header
    // sits a little below the top of the viewport, not at y=0. Step down to
    // find it, and probe a couple of x positions to dodge centred logos/search.
    var xs = [Math.round(innerWidth * 0.5), Math.round(innerWidth * 0.14)];
    [84, 100, 120, 150].forEach(function (y, i) {
      xs.forEach(function (x, j) { probe("TOP" + i + "_" + j, x, y); });
    });
    probe("MID", xs[0], Math.round(innerHeight / 2));
  };

  /// Drops the inline backgrounds we put on <html>/<body>.
  function clearRoot() {
    [document.documentElement, document.body].forEach(function (e) {
      if (!e) return;
      e.style.removeProperty("background-color");
      e.style.removeProperty("background-image");
    });
  }

  function paint() {
    restore();
    var tag = document.getElementById(STYLE_ID);
    if (!level) {
      clearRoot();
      if (tag) tag.remove();
      return;
    }
    if (!tag) {
      tag = document.createElement("style");
      tag.id = STYLE_ID;
      (document.head || document.documentElement).appendChild(tag);
    }
    tag.textContent = "";          // clear ours so the sample sees real colours
    clearRoot();
    captureBase();

    // Set on the elements themselves, not in our stylesheet. A site's own
    // sheet loads *after* ours, so with both carrying !important the site
    // wins on order -- which is exactly why YouTube stayed opaque. An inline
    // declaration outranks every stylesheet regardless of order.
    var root = document.documentElement;
    root.style.setProperty(
      "background-color",
      "rgba(" + base + "," + LEVELS[level].alpha + ")",
      "important"
    );
    root.style.setProperty("background-image", "none", "important");
    if (document.body) {
      document.body.style.setProperty("background-color", "transparent", "important");
      document.body.style.setProperty("background-image", "none", "important");
    }

    tag.textContent =
      "*,*::before,*::after{backdrop-filter:none!important;-webkit-backdrop-filter:none!important}";
    clearSurfaces(document.body || document.documentElement);
  }

  function settle() {
    clearTimeout(timer);
    timer = setTimeout(paint, 350);
  }

  // Pushes the page clear of the chrome bar across the top of the window.
  //
  // Padding alone is not enough: the content webview always fills the window,
  // and a site's `position: fixed` header would still anchor to the viewport
  // top, landing underneath our bar. A transform on <html> makes it the
  // containing block for fixed descendants, so those land below the bar too.
  function reserveHeader(px) {
    var root = document.documentElement;
    root.style.setProperty("transform", "translateY(" + px + "px)", "important");
    root.style.setProperty("height", "calc(100% - " + px + "px)", "important");
  }

  window.__prismGlass = function (n, chromeH) {
    level = Math.max(0, Math.min(LEVELS.length - 1, n | 0));
    if (chromeH) {
      chromeOffset = chromeH;
      reserveHeader(chromeH);
      grips(chromeH);
    }
    paint();
    send("diag", {
      msg: "glass=" + level +
        " base=" + base +
        " cleared=" + touched.length +
        " htmlbg=" + getComputedStyle(document.documentElement).backgroundColor
    });
  };

  // The chrome webview only covers the bottom strip, so the other window edges
  // have to offer their own resize grips from inside the page.
  // Chrome owns the top edges; the page supplies the rest. The host carries an
  // inverse translate because it sits inside the transformed <html> and would
  // otherwise be pushed down along with the page.
  function grips(offset) {
    var host = document.getElementById("__prism_grips");
    if (host) {
      host.style.setProperty("transform", "translateY(" + -offset + "px)", "important");
      return;
    }
    host = document.createElement("div");
    host.id = "__prism_grips";
    host.style.setProperty("transform", "translateY(" + -offset + "px)", "important");
    var sh = host.attachShadow({ mode: "closed" });
    var st = document.createElement("style");
    st.textContent =
      ":host{all:initial;position:fixed;left:0;top:0;width:100vw;height:100vh;" +
      "z-index:2147483647;pointer-events:none}" +
      ".g{position:absolute;pointer-events:auto}" +
      ".s{bottom:0;left:0;right:0;height:5px;cursor:ns-resize}" +
      ".w{left:0;top:0;bottom:0;width:5px;cursor:ew-resize}" +
      ".e{right:0;top:0;bottom:0;width:5px;cursor:ew-resize}" +
      ".sw{bottom:0;left:0;width:13px;height:13px;cursor:nesw-resize}" +
      ".se{bottom:0;right:0;width:13px;height:13px;cursor:nwse-resize}";
    sh.appendChild(st);
    ["s", "w", "e", "sw", "se"].forEach(function (d) {
      var g = document.createElement("div");
      g.className = "g " + d;
      g.addEventListener("mousedown", function (e) {
        if (e.button !== 0) return;
        e.preventDefault();
        e.stopPropagation();
        send("resize", { dir: d });
      });
      sh.appendChild(g);
    });
    document.documentElement.appendChild(host);
  }

  function start() {
    if (document.body) {
      new MutationObserver(function () { if (level) settle(); })
        .observe(document.body, { childList: true, subtree: true });
    }
    addEventListener("resize", function () { if (level) settle(); }, { passive: true });

    // Safety net: the mutation observer catches most changes, but a surface
    // that loads its background late, or a mutation that settled before the
    // element appeared, can slip through. A periodic re-sweep mops those up.
    // Cheap because paint() is a bounded walk and only runs while glass is on.
    setInterval(function () { if (level) paint(); }, 120000);

    send("ready");
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start, { once: true });
  } else {
    start();
  }
})();
