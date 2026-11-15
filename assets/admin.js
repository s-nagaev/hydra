// Admin UI helpers, self-hosted like admin.css and htmx.min.js: no build
// step, no dependencies, embedded into the binary with include_bytes!.
//
// Three behaviors live here. The log detail page shows captured bodies inside
// <pre class="code-snippet"> elements. A body is attacker-influenced input, so
// the highlighter reads the element with textContent, tokenizes the JSON in
// that string, and rebuilds the element out of text nodes and <span>
// elements. Raw body text never becomes an HTML string. Forms on the detail
// pages carry no htmx, so a destructive one names its question in
// data-confirm and the submit listener asks the browser's own confirm dialog
// before the request leaves. The header carries the theme toggle, which flips
// the stored choice the inline head script already applied.
(function () {
  "use strict";

  // One JSON token: a string (a key when a colon follows it, group 2), a
  // literal, a number, or a piece of punctuation. Everything between
  // matches is copied verbatim as plain text.
  var TOKEN = /("(?:\\u[0-9a-fA-F]{4}|\\[^u]|[^\\"\u0000-\u001f])*")(\s*:)?|\b(true|false|null)\b|(-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?)|([{}\[\],:])/g;

  function token(className, text) {
    var node = document.createElement("span");
    node.className = className;
    node.textContent = text;
    return node;
  }

  // Turns one JSON document into a fragment of styled text nodes.
  function highlightTokens(text) {
    var fragment = document.createDocumentFragment();
    var cursor = 0;
    var match;
    TOKEN.lastIndex = 0;
    while ((match = TOKEN.exec(text)) !== null) {
      if (match.index > cursor) {
        fragment.appendChild(document.createTextNode(text.slice(cursor, match.index)));
      }
      if (match[1] !== undefined) {
        if (match[2] !== undefined) {
          fragment.appendChild(token("json-key", match[1]));
          fragment.appendChild(token("json-punct", match[2]));
        } else {
          fragment.appendChild(token("json-string", match[1]));
        }
      } else if (match[3] !== undefined) {
        fragment.appendChild(token("json-literal", match[3]));
      } else if (match[4] !== undefined) {
        fragment.appendChild(token("json-number", match[4]));
      } else if (match[5] !== undefined) {
        fragment.appendChild(token("json-punct", match[5]));
      }
      cursor = TOKEN.lastIndex;
    }
    if (cursor < text.length) {
      fragment.appendChild(document.createTextNode(text.slice(cursor)));
    }
    return fragment;
  }

  // Not JSON (an SSE capture, a truncated body, an error page): the stored
  // text stays exactly as the server rendered it.
  function highlight(pre) {
    var value;
    try {
      value = JSON.parse(pre.textContent);
    } catch (error) {
      return;
    }
    pre.textContent = "";
    pre.appendChild(highlightTokens(JSON.stringify(value, null, 2)));
  }

  function run() {
    var bodies = document.querySelectorAll("pre.code-snippet");
    for (var index = 0; index < bodies.length; index++) {
      highlight(bodies[index]);
    }
  }

  // The theme toggle of the header. The inline script in <head> has already
  // applied the stored theme and written the cookie the stats pages read, so
  // this only has to keep the button in step and to handle the flip. A page
  // carrying server-rendered charts reloads once: the SVG colors are chosen
  // on the server, and the reload is what gives the new theme's palette to
  // the bars, the axes and the legend of the page already on screen.
  function applyTheme(theme) {
    document.documentElement.setAttribute("data-theme", theme);
    try {
      window.localStorage.setItem("hydra-theme", theme);
    } catch (error) {
      // A browser that refuses storage keeps the theme of this page anyway.
    }
    document.cookie = "theme=" + theme + "; path=/; max-age=31536000; samesite=lax";
    var button = document.getElementById("theme-toggle");
    if (button) {
      var dark = theme === "dark";
      button.setAttribute("aria-pressed", dark ? "true" : "false");
      button.setAttribute("aria-label", dark ? "Switch to light theme" : "Switch to dark theme");
    }
  }

  function activeTheme() {
    return document.documentElement.getAttribute("data-theme") === "dark" ? "dark" : "light";
  }

  function initToggle() {
    var button = document.getElementById("theme-toggle");
    if (!button) {
      return;
    }
    applyTheme(activeTheme());
    button.addEventListener("click", function () {
      applyTheme(activeTheme() === "dark" ? "light" : "dark");
      if (document.querySelector(".chart-block")) {
        window.location.reload();
      }
    });
  }

  function start() {
    run();
    initToggle();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start);
  } else {
    start();
  }

  // Confirmation for a destructive form on a page without htmx: the form
  // carries the question, the browser handles the dialog, a declined answer
  // cancels the submit.
  document.addEventListener("submit", function (event) {
    var form = event.target;
    if (!(form instanceof HTMLFormElement)) {
      return;
    }
    var message = form.getAttribute("data-confirm");
    if (message && !window.confirm(message)) {
      event.preventDefault();
    }
  });
})();
