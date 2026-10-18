/* open-rust-chatbot widget. MIT licensed. https://github.com/blueswansconsulting/open-rust-chatbot
 *
 * Embed:  <script src="https://chat.example.com/widget.js" defer></script>
 * Optional config, set before the script tag:
 *   window.openRustChatbot = { title, subtitle, greeting, placeholder, serverUrl, turnstileToken };
 * turnstileToken may be a string or a function returning a fresh token (or a Promise of one);
 * Turnstile tokens are single-use, so use a function if the visitor may reconnect.
 *
 * Security: every piece of text from the server or visitor is rendered with textContent, never innerHTML.
 */
(function () {
  'use strict';
  if (window.__openRustChatbotLoaded) return;
  window.__openRustChatbotLoaded = true;

  var options = window.openRustChatbot || {};
  var script = document.currentScript;
  var serverOrigin;
  try {
    serverOrigin = new URL(options.serverUrl || (script && script.src) || location.href).origin;
  } catch (e) {
    serverOrigin = location.origin;
  }
  var wsBase = (serverOrigin.indexOf('https:') === 0 ? 'wss:' : 'ws:') + '//' + new URL(serverOrigin).host + '/ws';

  function wsUrl() {
    var token = options.turnstileToken;
    return Promise.resolve(typeof token === 'function' ? token() : token).then(function (t) {
      return t ? wsBase + '?ts=' + encodeURIComponent(t) : wsBase;
    });
  }

  var TEXT = {
    title: options.title || 'Chat with us',
    subtitle: options.subtitle || 'Usually replies instantly',
    greeting: options.greeting || 'Hi! How can I help you today?',
    placeholder: options.placeholder || 'Type your message...',
  };

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  var URL_RE = /https?:\/\/[^\s<>"']+/g;

  // Turns http(s) URLs in a finished message into links, using DOM nodes only (never innerHTML).
  function linkify(node) {
    var text = node.textContent || '';
    var parts = [];
    var last = 0;
    var match;
    URL_RE.lastIndex = 0;
    while ((match = URL_RE.exec(text))) {
      var raw = match[0].replace(/[.,;:!?)\]]+$/, '');
      var url;
      try {
        url = new URL(raw);
      } catch (e) {
        continue;
      }
      if (url.protocol !== 'https:' && url.protocol !== 'http:') continue;
      parts.push(document.createTextNode(text.slice(last, match.index)));
      var a = document.createElement('a');
      a.href = url.href;
      a.textContent = raw;
      a.rel = 'noopener noreferrer nofollow';
      if (url.host !== location.host) a.target = '_blank';
      parts.push(a);
      last = match.index + raw.length;
      URL_RE.lastIndex = last;
    }
    if (!parts.length) return;
    parts.push(document.createTextNode(text.slice(last)));
    node.textContent = '';
    parts.forEach(function (p) {
      node.appendChild(p);
    });
  }

  function chatIcon() {
    var ns = 'http://www.w3.org/2000/svg';
    var svg = document.createElementNS(ns, 'svg');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('aria-hidden', 'true');
    var path = document.createElementNS(ns, 'path');
    path.setAttribute('d', 'M4 4h16a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H9l-5 4v-4H4a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2z');
    path.setAttribute('fill', 'currentColor');
    svg.appendChild(path);
    return svg;
  }

  function mount() {
    if (!document.querySelector('link[data-orc-stylesheet]')) {
      var link = document.createElement('link');
      link.rel = 'stylesheet';
      link.href = serverOrigin + '/widget.css';
      link.setAttribute('data-orc-stylesheet', '');
      document.head.appendChild(link);
    }

    var root = el('div', 'orc-root');
    var bubble = el('button', 'orc-bubble');
    bubble.type = 'button';
    bubble.setAttribute('aria-label', 'Open chat');
    bubble.setAttribute('data-testid', 'orc-bubble');
    bubble.appendChild(chatIcon());

    var panel = el('div', 'orc-panel');
    panel.dataset.open = 'false';
    panel.setAttribute('role', 'dialog');
    panel.setAttribute('aria-label', TEXT.title);

    var header = el('div', 'orc-header');
    var headerText = el('div', 'orc-header-text');
    headerText.appendChild(el('div', 'orc-header-title', TEXT.title));
    headerText.appendChild(el('div', 'orc-header-subtitle', TEXT.subtitle));
    var closeBtn = el('button', 'orc-close', '×');
    closeBtn.type = 'button';
    closeBtn.setAttribute('aria-label', 'Close chat');
    header.appendChild(headerText);
    header.appendChild(closeBtn);

    var messages = el('div', 'orc-messages');
    messages.setAttribute('aria-live', 'polite');
    messages.setAttribute('data-testid', 'orc-messages');

    var inputRow = el('form', 'orc-input-row');
    var input = el('input', 'orc-input');
    input.type = 'text';
    input.maxLength = 2000;
    input.placeholder = TEXT.placeholder;
    input.setAttribute('aria-label', TEXT.placeholder);
    var sendBtn = el('button', 'orc-send', 'Send');
    sendBtn.type = 'submit';
    inputRow.appendChild(input);
    inputRow.appendChild(sendBtn);

    panel.appendChild(header);
    panel.appendChild(messages);
    panel.appendChild(inputRow);
    root.appendChild(panel);
    root.appendChild(bubble);
    document.body.appendChild(root);

    var ws = null;
    var connecting = false;
    var queue = [];
    var current = null;
    var typing = null;
    var waiting = false;
    var greeted = false;
    var lastText = null;
    var awaitingSignIn = false;

    function scroll() {
      messages.scrollTop = messages.scrollHeight;
    }

    function append(role, text) {
      var div = el('div', 'orc-msg orc-msg-' + role, text);
      div.setAttribute('data-role', role);
      messages.appendChild(div);
      scroll();
      return div;
    }

    function setWaiting(value) {
      waiting = value;
      sendBtn.disabled = value;
      if (value && !typing) {
        typing = el('div', 'orc-typing');
        typing.setAttribute('aria-label', 'Assistant is typing');
        for (var i = 0; i < 3; i++) typing.appendChild(el('span'));
        messages.appendChild(typing);
        scroll();
      } else if (!value && typing) {
        typing.remove();
        typing = null;
      }
    }

    function leadCard(card) {
      var lead = card.lead || {};
      var wrap = el('div', 'orc-card');
      var submitted = card.type === 'lead_submitted';
      wrap.appendChild(el('div', 'orc-card-title', submitted ? 'Details sent' : 'Send these details to the team?'));
      [lead.name, lead.email, lead.phone, lead.message].forEach(function (line) {
        if (line) wrap.appendChild(el('div', 'orc-card-line', String(line)));
      });
      if (!submitted) {
        var confirm = el('button', 'orc-card-action', 'Confirm and send');
        confirm.type = 'button';
        confirm.addEventListener('click', function () {
          confirm.disabled = true;
          setWaiting(true);
          send(JSON.stringify({ type: 'confirm_lead', leadId: String(lead.leadId || '') }));
        });
        wrap.appendChild(confirm);
      }
      messages.appendChild(wrap);
      scroll();
    }

    // "Sign in with ChatGPT" mode: the visitor's own ChatGPT plan pays for their replies.
    function signInCard(path) {
      if (awaitingSignIn) return;
      awaitingSignIn = true;
      var wrap = el('div', 'orc-card');
      wrap.appendChild(el('div', 'orc-card-title', 'Sign in to chat'));
      wrap.appendChild(el('div', 'orc-card-line', 'This assistant uses your own ChatGPT plan. Sign in with ChatGPT to continue.'));
      var button = el('button', 'orc-card-action', 'Sign in with ChatGPT');
      button.type = 'button';
      button.addEventListener('click', function () {
        var url = new URL(path, serverOrigin);
        if (url.origin !== serverOrigin) return;
        window.open(url.href, 'orc-signin', 'width=520,height=720');
      });
      wrap.appendChild(button);
      messages.appendChild(wrap);
      scroll();
    }

    window.addEventListener('message', function (event) {
      if (event.origin !== serverOrigin || !event.data || event.data.type !== 'orc-signed-in') return;
      if (!awaitingSignIn) return;
      awaitingSignIn = false;
      append('assistant', "You're signed in.");
      // Reconnect so the new socket carries the sign-in cookie, then resend the question.
      if (ws) ws.close();
      ws = null;
      if (lastText) {
        setWaiting(true);
        send(lastText);
      }
    });

    function onFrame(event) {
      var frame;
      try {
        frame = JSON.parse(event.data);
      } catch (e) {
        return;
      }
      if (frame.type === 'token') {
        if (typing) {
          typing.remove();
          typing = null;
        }
        if (!current) current = append('assistant', '');
        current.textContent += String(frame.value);
        scroll();
      } else if (frame.type === 'done') {
        if (current) linkify(current);
        current = null;
        setWaiting(false);
      } else if (frame.type === 'error') {
        current = null;
        setWaiting(false);
        append('error', String(frame.message));
      } else if (frame.type === 'auth_required') {
        current = null;
        signInCard(String(frame.url || '/auth/chatgpt/start'));
      } else if (frame.type === 'card' && frame.card) {
        current = null;
        leadCard(frame.card);
      }
    }

    function connect() {
      if (connecting || (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING))) return;
      connecting = true;
      wsUrl().then(
        function (url) {
          connecting = false;
          var socket = new WebSocket(url);
          ws = socket;
          var opened = false;
          socket.addEventListener('open', function () {
            opened = true;
            while (queue.length) socket.send(queue.shift());
          });
          socket.addEventListener('message', onFrame);
          socket.addEventListener('close', function () {
            // A socket we replaced on purpose (after sign-in) must not touch the new one's state.
            if (ws !== socket) return;
            // Never replay messages from a failed attempt on a later connection.
            queue = [];
            // Confirm buttons belong to the old session; the server has forgotten those cards.
            messages.querySelectorAll('.orc-card-action').forEach(function (b) {
              b.disabled = true;
            });
            if (waiting || !opened) {
              setWaiting(false);
              append('error', opened ? 'Connection lost. Please try again.' : 'Could not connect. Please try again.');
            }
            current = null;
            ws = null;
          });
        },
        function () {
          connecting = false;
          queue = [];
          setWaiting(false);
          append('error', 'Could not connect. Please try again.');
        }
      );
    }

    function send(payload) {
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(payload);
        return;
      }
      queue.push(payload);
      connect();
    }

    function open() {
      panel.dataset.open = 'true';
      bubble.setAttribute('aria-expanded', 'true');
      if (!greeted) {
        greeted = true;
        append('assistant', TEXT.greeting);
      }
      connect();
      input.focus();
    }

    function close() {
      panel.dataset.open = 'false';
      bubble.setAttribute('aria-expanded', 'false');
      bubble.focus();
    }

    bubble.setAttribute('aria-expanded', 'false');
    bubble.addEventListener('click', function () {
      if (panel.dataset.open === 'true') close();
      else open();
    });
    closeBtn.addEventListener('click', close);
    panel.addEventListener('keydown', function (e) {
      if (e.key === 'Escape') close();
    });
    inputRow.addEventListener('submit', function (e) {
      e.preventDefault();
      var text = input.value.trim();
      if (!text || waiting) return;
      append('user', text);
      input.value = '';
      lastText = text;
      setWaiting(true);
      send(text);
    });
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', mount, { once: true });
  else mount();
})();
