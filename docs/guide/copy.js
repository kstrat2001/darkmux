// Copy-to-clipboard for guide code/shell blocks.
//
// Auto-augments every `<pre>` block: wraps it in a `.code-wrap` (so the button
// can pin to the visible top-right regardless of the block's horizontal scroll)
// and appends a "Copy" button. One delegated click handler copies the block's
// code text. The page's code is static authored content — no untrusted input.
(function () {
  "use strict";

  function augment() {
    var blocks = document.querySelectorAll("pre");
    for (var i = 0; i < blocks.length; i++) {
      var pre = blocks[i];
      // Skip if already wrapped (idempotent — safe to re-run).
      if (pre.parentElement && pre.parentElement.classList.contains("code-wrap")) {
        continue;
      }
      var wrap = document.createElement("div");
      wrap.className = "code-wrap";
      pre.parentNode.insertBefore(wrap, pre);
      wrap.appendChild(pre);

      var btn = document.createElement("button");
      btn.type = "button";
      btn.className = "copy-btn";
      btn.textContent = "Copy";
      btn.setAttribute("aria-label", "Copy to clipboard");
      wrap.appendChild(btn);
    }
  }

  function textFor(pre) {
    // The button is a sibling of <pre> (inside .code-wrap), so the <pre>/<code>
    // text never includes the button label.
    var code = pre.querySelector("code");
    var text = (code ? code.textContent : pre.textContent) || "";
    return text.replace(/\s+$/, "");
  }

  document.addEventListener("click", function (e) {
    var btn = e.target.closest(".copy-btn");
    if (!btn) return;
    var wrap = btn.closest(".code-wrap");
    var pre = wrap ? wrap.querySelector("pre") : null;
    if (!pre) return;

    var reset = function (label, ok) {
      btn.textContent = label;
      btn.classList.toggle("copied", ok);
      setTimeout(function () {
        btn.textContent = "Copy";
        btn.classList.remove("copied");
      }, 1200);
    };

    var text = textFor(pre);
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(
        function () { reset("Copied", true); },
        function () { reset("Failed", false); }
      );
    } else {
      reset("Failed", false);
    }
  });

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", augment);
  } else {
    augment();
  }
})();
