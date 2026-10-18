/* open-rust-chatbot widget. MIT licensed. https://github.com/blueswansconsulting/open-rust-chatbot
 *
 * Embed:  <script src="https://chat.example.com/widget.js" defer></script>
 * Optional config, set before the script tag:
 *   window.openRustChatbot = { title, subtitle, greeting, placeholder, serverUrl, turnstileToken };
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
  var wsUrl = (serverOrigin.indexOf('https:') === 0 ? 'wss:' : 'ws:') + '//' + new URL(serverOrigin).host + '/ws';
  if (options.turnstileToken) wsUrl += '?ts=' + encodeURIComponent(options.turnstileToken);

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
    var queue = [];
    var current = null;
    var typing = null;
    var waiting = false;
    var greeted = false;

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
        current = null;
        setWaiting(false);
      } else if (frame.type === 'error') {
        current = null;
        setWaiting(false);
        append('error', String(frame.message));
      } else if (frame.type === 'card' && frame.card) {
        current = null;
        leadCard(frame.card);
      }
    }

    function connect() {
      if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) return;
      ws = new WebSocket(wsUrl);
      ws.addEventListener('open', function () {
        while (queue.length) ws.send(queue.shift());
      });
      ws.addEventListener('message', onFrame);
      ws.addEventListener('close', function () {
        if (waiting) {
          setWaiting(false);
          append('error', 'Connection lost. Please try again.');
        }
        current = null;
        ws = null;
      });
    }

    function send(payload) {
      connect();
      if (ws.readyState === WebSocket.OPEN) ws.send(payload);
      else queue.push(payload);
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

    bubble.addEventListener('click', function () {
      if (panel.dataset.open === 'true') panel.dataset.open = 'false';
      else open();
    });
    closeBtn.addEventListener('click', function () {
      panel.dataset.open = 'false';
      bubble.setAttribute('aria-expanded', 'false');
      bubble.focus();
    });
    inputRow.addEventListener('submit', function (e) {
      e.preventDefault();
      var text = input.value.trim();
      if (!text || waiting) return;
      append('user', text);
      input.value = '';
      setWaiting(true);
      send(text);
    });
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', mount, { once: true });
  else mount();
})();
