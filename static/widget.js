/* open-rust-chatbot widget. MIT licensed. https://github.com/blueswansconsulting/open-rust-chatbot
 *
 * Embed:  <script src="https://chat.example.com/widget.js" defer></script>
 * Optional config, set before the script tag:
 *   window.openRustChatbot = { title, subtitle, greeting, placeholder, serverUrl, turnstileToken,
 *                              avatar, avatarMotion };
 * serverUrl may include a path when the server is behind a sub-path, e.g. https://example.com/chat
 * Any of title, subtitle, greeting, placeholder and privacyUrl left out are read once from
 * <base>/config when the server provides it (multi-site mode); options set here always win.
 * avatar names one of the bundled mascots (see /avatars/, e.g. 'electrician') for the launcher and
 * header; avatarMotion is 'still', 'wave' (once, after load) or 'follow' (turns toward the pointer).
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
  // The server may live at a sub-path (e.g. https://example.com/chat/widget.js), so derive the
  // base from where this script was loaded: <base>/ws and <base>/widget.css.
  var serverOrigin;
  var serverBase;
  try {
    var src = new URL(options.serverUrl || (script && script.src) || location.origin + '/widget.js');
    serverOrigin = src.origin;
    var path = options.serverUrl ? src.pathname : src.pathname.replace(/\/[^\/]*$/, '');
    serverBase = serverOrigin + path.replace(/\/+$/, '');
  } catch (e) {
    serverOrigin = location.origin;
    serverBase = serverOrigin;
  }
  var wsBase = serverBase.replace(/^http/, 'ws') + '/ws';

  function wsUrl() {
    var token = options.turnstileToken;
    return Promise.resolve(typeof token === 'function' ? token() : token).then(function (t) {
      return t ? wsBase + '?ts=' + encodeURIComponent(t) : wsBase;
    });
  }

  var DEFAULTS = {
    title: 'Chat with us',
    subtitle: 'Usually replies instantly',
    greeting: 'Hi! How can I help you today?',
    placeholder: 'Type your message...',
  };
  var CONFIG_KEYS = ['title', 'subtitle', 'greeting', 'placeholder', 'privacyUrl'];
  var TEXT = {
    // Required transparency (EU AI Act Art. 50): visitors must know they're talking to an AI.
    disclosure: options.disclosure || "You're chatting with an AI assistant. It can make mistakes, so please check anything important.",
  };
  var privacyOption = options.privacyUrl;
  var AVATAR_NAME = /^[a-z]{2,20}$/;
  var MOTIONS = ['still', 'wave', 'follow'];
  // Bundled mascots live at the server root, also for bots served from /b/<id>/.
  var avatarBase = serverBase.replace(/\/b\/[a-z0-9-]+$/, '') + '/avatars/';

  // Settings the host page left out may come from the server. The widget mounts straight away
  // with the defaults and updates its texts when (and if) the reply arrives; any failure,
  // including a 404 from an older server, keeps the defaults.
  function applySettings(remote) {
    remote = remote && typeof remote === 'object' ? remote : {};
    function pick(key) {
      if (options[key]) return options[key];
      return typeof remote[key] === 'string' && remote[key] ? remote[key] : DEFAULTS[key];
    }
    TEXT.title = pick('title');
    TEXT.subtitle = pick('subtitle');
    TEXT.greeting = pick('greeting');
    TEXT.placeholder = pick('placeholder');
    if (!privacyOption && typeof remote.privacyUrl === 'string') privacyOption = remote.privacyUrl;
    // The host page may pick a mascot, or turn the bot's off with avatar: false or 'none'.
    var avatar = options.avatar !== undefined ? options.avatar : remote.avatar;
    TEXT.avatar = typeof avatar === 'string' && avatar !== 'none' && AVATAR_NAME.test(avatar) ? avatar : null;
    var motion = options.avatarMotion || remote.avatarMotion;
    TEXT.avatarMotion = MOTIONS.indexOf(motion) >= 0 ? motion : 'still';
  }

  function loadSettings() {
    var missing =
      options.avatar === undefined ||
      CONFIG_KEYS.some(function (k) {
        return !options[k];
      });
    if (!missing || typeof fetch !== 'function') return Promise.resolve(null);
    return fetch(serverBase + '/config', { credentials: 'omit' })
      .then(function (r) {
        return r.ok ? r.json() : null;
      })
      .catch(function () {
        return null;
      });
  }

  function safeUrl(value) {
    try {
      var u = new URL(String(value), location.href);
      return u.protocol === 'https:' || u.protocol === 'http:' ? u.href : null;
    } catch (e) {
      return null;
    }
  }

  // Per-tab storage for continuing a chat on the next page; never required to work.
  var store = {
    get: function (key) {
      try {
        return sessionStorage.getItem('orc-' + key);
      } catch (e) {
        return null;
      }
    },
    set: function (key, value) {
      try {
        if (value === null) sessionStorage.removeItem('orc-' + key);
        else sessionStorage.setItem('orc-' + key, value);
      } catch (e) {}
    },
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
      var bare = function (h) {
        return h.replace(/^www\./, '');
      };
      if (bare(url.host) !== bare(location.host)) a.target = '_blank';
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

  // The mascot as inline SVG, so its parts can move. Only the bundled files from our own server
  // are fetched, and only plain shapes and their drawing attributes are copied into the page.
  var SVG_NS = 'http://www.w3.org/2000/svg';
  var SVG_TAGS = ['svg', 'g', 'path', 'circle', 'ellipse', 'rect', 'line', 'polygon', 'polyline'];
  var SVG_ATTRS = [
    'viewBox', 'width', 'height', 'class', 'transform', 'opacity', 'fill', 'fill-opacity', 'fill-rule',
    'stroke', 'stroke-width', 'stroke-linecap', 'stroke-linejoin', 'stroke-opacity',
    'd', 'cx', 'cy', 'r', 'rx', 'ry', 'x', 'y', 'x1', 'y1', 'x2', 'y2', 'points',
  ];

  function cleanSvg(node) {
    if (node.namespaceURI !== SVG_NS || SVG_TAGS.indexOf(node.localName) < 0) return null;
    var copy = document.createElementNS(SVG_NS, node.localName);
    SVG_ATTRS.forEach(function (name) {
      var value = node.getAttribute(name);
      if (value !== null && !/url\s*\(/i.test(value)) copy.setAttribute(name, value);
    });
    [].slice.call(node.children).forEach(function (child) {
      var clean = cleanSvg(child);
      if (clean) copy.appendChild(clean);
    });
    return copy;
  }

  function inlineAvatar(name) {
    if (typeof fetch !== 'function' || typeof DOMParser !== 'function') return Promise.resolve(null);
    return fetch(avatarBase + name + '.svg', { credentials: 'omit' })
      .then(function (r) {
        return r.ok ? r.text() : null;
      })
      .then(function (text) {
        if (!text) return null;
        var doc = new DOMParser().parseFromString(text, 'image/svg+xml');
        if (doc.getElementsByTagName('parsererror').length) return null;
        var svg = doc.documentElement && doc.documentElement.localName === 'svg' ? cleanSvg(doc.documentElement) : null;
        if (!svg) return null;
        svg.setAttribute('aria-hidden', 'true');
        svg.setAttribute('focusable', 'false');
        return svg;
      })
      .catch(function () {
        return null;
      });
  }

  function avatarImg(name, onError) {
    var img = el('img');
    img.alt = '';
    img.src = avatarBase + name + '.svg';
    img.addEventListener('error', onError);
    return img;
  }

  // Head turns toward the pointer: it shifts and tilts a little and the face shifts a bit more
  // than the hat, which fakes a 3D turn. Frames run only while the head is moving; at rest the
  // gentle bob and blinks are CSS. Visitors who ask for reduced motion get a still mascot.
  function followPointer(svg) {
    var head = svg.querySelector('.orc-head');
    var face = svg.querySelector('.orc-face');
    var hat = svg.querySelector('.orc-hat');
    if (!head || !face || !hat || !window.requestAnimationFrame) return;
    var reduce = window.matchMedia ? window.matchMedia('(prefers-reduced-motion: reduce)') : { matches: false };
    var target = { x: 0, y: 0 };
    var cur = { x: 0, y: 0 };
    var lastMove = 0;
    var running = false;
    var rect = null;
    function clear() {
      [head, face, hat].forEach(function (n) {
        n.removeAttribute('transform');
      });
      cur.x = cur.y = 0;
    }
    function aim(x, y) {
      if (reduce.matches) return;
      // One layout read per frame, however many pointer events arrive.
      if (!rect) rect = svg.getBoundingClientRect();
      var clamp = function (v) {
        return Math.max(-1, Math.min(1, v));
      };
      target.x = clamp((x - (rect.left + rect.width / 2)) / (window.innerWidth / 2));
      target.y = clamp((y - (rect.top + rect.height / 2)) / (window.innerHeight / 2));
      lastMove = Date.now();
      if (!running) {
        running = true;
        requestAnimationFrame(frame);
      }
    }
    function frame() {
      rect = null;
      if (reduce.matches || !svg.isConnected) {
        running = false;
        return clear();
      }
      var idle = Date.now() - lastMove > 2500;
      if (idle) target.x = target.y = 0;
      cur.x += (target.x - cur.x) * 0.12;
      cur.y += (target.y - cur.y) * 0.12;
      if (idle && Math.abs(cur.x) < 0.005 && Math.abs(cur.y) < 0.005) {
        running = false;
        return clear();
      }
      var x = cur.x;
      var y = cur.y;
      head.setAttribute('transform', 'translate(' + (x * 1.6).toFixed(2) + ' ' + (y * 1.2).toFixed(2) + ') rotate(' + (x * 6).toFixed(2) + ' 32 37)');
      face.setAttribute('transform', 'translate(' + (x * 3.2).toFixed(2) + ' ' + (y * 2.6).toFixed(2) + ')');
      hat.setAttribute('transform', 'translate(' + (x * 1.2).toFixed(2) + ' ' + (y * 0.8).toFixed(2) + ')');
      requestAnimationFrame(frame);
    }
    window.addEventListener('pointermove', function (e) {
      aim(e.clientX, e.clientY);
    }, { passive: true });
    window.addEventListener('touchmove', function (e) {
      if (e.touches[0]) aim(e.touches[0].clientX, e.touches[0].clientY);
    }, { passive: true });
  }

  function mount() {
    if (!document.querySelector('link[data-orc-stylesheet]')) {
      var link = document.createElement('link');
      link.rel = 'stylesheet';
      link.href = serverBase + '/widget.css';
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
    var titleNode = el('div', 'orc-header-title', TEXT.title);
    var subtitleNode = el('div', 'orc-header-subtitle', TEXT.subtitle);
    headerText.appendChild(titleNode);
    headerText.appendChild(subtitleNode);
    var avatarShown = false;
    // Shows the bot's mascot once its name is known; any failure keeps the plain chat icon.
    function showAvatar() {
      var name = TEXT.avatar;
      if (!name || avatarShown) return;
      avatarShown = true;
      var restore = function () {
        bubble.classList.remove('orc-bubble-avatar');
        bubble.textContent = '';
        bubble.appendChild(chatIcon());
        headerAvatar.remove();
      };
      var headerAvatar = el('div', 'orc-header-avatar');
      headerAvatar.appendChild(avatarImg(name, function () {
        headerAvatar.remove();
      }));
      header.insertBefore(headerAvatar, headerText);
      var motion = TEXT.avatarMotion;
      var still = function () {
        bubble.textContent = '';
        bubble.classList.add('orc-bubble-avatar');
        bubble.appendChild(avatarImg(name, restore));
      };
      if (motion === 'still') return still();
      inlineAvatar(name).then(function (svg) {
        if (!svg) return still();
        bubble.textContent = '';
        bubble.classList.add('orc-bubble-avatar');
        bubble.appendChild(svg);
        if (motion === 'follow') {
          root.classList.add('orc-follow');
          return followPointer(svg);
        }
        // Wave once per tab, not on every page, and never again when the button reappears.
        var hand = svg.querySelector('.orc-hand');
        if (store.get('waved') || !hand) return;
        hand.addEventListener('animationend', function () {
          store.set('waved', '1');
          root.classList.remove('orc-wave-go');
        });
        root.classList.add('orc-wave-go');
      });
    }
    var closeBtn = el('button', 'orc-close', '×');
    closeBtn.type = 'button';
    closeBtn.setAttribute('aria-label', 'Close chat');
    header.appendChild(headerText);
    header.appendChild(closeBtn);

    var messages = el('div', 'orc-messages');
    messages.setAttribute('data-testid', 'orc-messages');
    // Screen readers hear each finished reply once, not every streamed word.
    var announcer = el('div', 'orc-sr-only');
    announcer.setAttribute('aria-live', 'polite');
    announcer.setAttribute('role', 'status');

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
    var disclosure = el('div', 'orc-disclosure', TEXT.disclosure);
    var privacyLink = null;
    function showPrivacyLink() {
      var privacyUrl = privacyOption && safeUrl(privacyOption);
      if (!privacyUrl || privacyLink) return;
      disclosure.appendChild(document.createTextNode(' '));
      privacyLink = el('a', 'orc-disclosure-link', 'Privacy');
      privacyLink.href = privacyUrl;
      privacyLink.rel = 'noopener noreferrer';
      disclosure.appendChild(privacyLink);
    }
    showPrivacyLink();
    panel.appendChild(disclosure);
    panel.appendChild(messages);
    panel.appendChild(inputRow);
    panel.appendChild(announcer);
    root.appendChild(panel);
    root.appendChild(bubble);
    // First in the page so keyboard users reach the chat without tabbing through every link.
    // Hidden until the server confirms the chat can answer (see checkHealth below).
    root.hidden = true;
    document.body.insertBefore(root, document.body.firstChild);

    var ws = null;
    var connecting = false;
    var queue = [];
    var current = null;
    var typing = null;
    var waiting = false;
    var greeted = false;
    var greetingNode = null;
    var replyTimer = null;
    var REPLY_TIMEOUT_MS = 60000;
    var SLOW_REPLY_MS = 15000;
    var slowTimer = null;

    function announce(text) {
      announcer.textContent = '';
      setTimeout(function () {
        announcer.textContent = text;
      }, 50);
    }

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

    // The visible conversation (not errors or cards), kept per tab so the next page continues it.
    var MAX_SAVED_MESSAGES = 40;
    function saveTranscript() {
      var saved = [];
      messages.querySelectorAll('.orc-msg-user, .orc-msg-assistant').forEach(function (m) {
        if (!m.textContent) return;
        saved.push({ r: m.getAttribute('data-role'), t: m.textContent });
      });
      store.set('transcript', JSON.stringify(saved.slice(-MAX_SAVED_MESSAGES)));
    }

    function restoreTranscript() {
      var saved;
      try {
        saved = JSON.parse(store.get('transcript') || '[]');
      } catch (e) {
        saved = [];
      }
      if (!Array.isArray(saved) || !saved.length) return false;
      saved.slice(-MAX_SAVED_MESSAGES).forEach(function (m) {
        if (!m || (m.r !== 'user' && m.r !== 'assistant')) return;
        var div = append(m.r, String(m.t || ''));
        div.setAttribute('data-restored', '');
        if (m.r === 'assistant') linkify(div);
      });
      return true;
    }

    function setWaiting(value) {
      waiting = value;
      sendBtn.disabled = value;
      clearTimeout(replyTimer);
      clearTimeout(slowTimer);
      if (value) {
        // Busy times queue replies; tell the visitor we're still on it.
        slowTimer = setTimeout(function () {
          if (waiting && typing && !typing.querySelector('.orc-typing-note')) {
            typing.appendChild(el('em', 'orc-typing-note', 'Still working on it…'));
            scroll();
          }
        }, SLOW_REPLY_MS);
        // Never leave the visitor watching typing dots forever.
        replyTimer = setTimeout(function () {
          if (!waiting) return;
          setWaiting(false);
          current = null;
          append('error', "Sorry, no reply came back. Please try again.");
          announce("Sorry, no reply came back. Please try again.");
          if (ws) {
            closingOnPurpose = ws;
            ws.close();
          }
        }, REPLY_TIMEOUT_MS);
      }
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
      [lead.name, lead.phone, lead.email, lead.bestTime ? 'Best time: ' + lead.bestTime : null, lead.message].forEach(function (line) {
        if (line) wrap.appendChild(el('div', 'orc-card-line', String(line)));
      });
      // Only the newest card can be confirmed.
      messages.querySelectorAll('.orc-card-action').forEach(function (b) {
        b.disabled = true;
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
      } else if (frame.type === 'session') {
        if (typeof frame.token === 'string') store.set('token', frame.token);
        // The server couldn't continue the earlier chat (expired or restarted), so don't show
        // a conversation the assistant no longer remembers.
        if (frame.resumed === false && restoredFromStorage) {
          restoredFromStorage = false;
          // Remove only the restored messages, so anything typed meanwhile stays.
          messages.querySelectorAll('[data-restored]').forEach(function (m) {
            m.remove();
          });
          var notice = append('notice', 'Your earlier chat has ended, so this is a new conversation.');
          var greeting = append('assistant', TEXT.greeting);
          messages.insertBefore(greeting, messages.firstChild);
          messages.insertBefore(notice, greeting);
          saveTranscript();
        }
        restoredFromStorage = false;
      } else if (frame.type === 'done') {
        if (current) {
          linkify(current);
          announce(current.textContent);
        }
        current = null;
        setWaiting(false);
        saveTranscript();
      } else if (frame.type === 'error') {
        current = null;
        setWaiting(false);
        append('error', String(frame.message));
        announce(String(frame.message));
      } else if (frame.type === 'card' && frame.card) {
        current = null;
        leadCard(frame.card);
      }
    }

    // ---- Availability -------------------------------------------------------------------
    // The button only shows while the chat can answer. If the connection drops mid-chat the
    // visitor gets one apology, the input pauses, and the widget reconnects by itself; it only
    // says "reconnected" once a new connection has actually opened.
    var available = false;
    var refused = false; // the server keeps refusing connections although it is up
    var refusedAt = 0;
    var lastCheckAt = 0;
    var REFUSAL_COOLDOWN_MS = 300000;
    var outage = false;
    var lostReply = false;
    var connectFailures = 0;
    var healthFailures = 0;
    var recoveryTimer = null;
    var recoveryDelay = 0;
    var healthTimer = null;
    var warnedUnreachable = false;
    var closingOnPurpose = null;
    var generation = 0;
    var HEALTH_RECHECK_MS = 120000;
    var HEALTH_RECHECK_DOWN_MS = 30000;
    var MSG_LOST_REPLY = "Sorry, we lost the connection before your reply arrived. We're reconnecting now.";
    var MSG_DROPPED = "Sorry, the chat connection has dropped. We're trying to reconnect.";
    var MSG_UNAVAILABLE = "Sorry, the chat isn't available right now. We'll keep trying and let you know when it's back.";
    var MSG_OFFLINE = "Sorry, you seem to be offline. We'll reconnect when your connection is back.";
    var MSG_CANT_CONNECT = "Sorry, the chat can't connect right now. Please try again later.";
    var MSG_NOT_SENT = "Sorry, we couldn't send your message because the chat can't connect right now. Please try again later.";
    var MSG_BACK = "You're reconnected. Please send your message again.";
    var MSG_BACK_IDLE = "You're reconnected. Carry on whenever you're ready.";

    // System notices (connection status) are styled apart and never saved in the transcript.
    function say(role, text) {
      append(role === 'assistant' ? 'notice' : role, text);
      announce(text);
    }

    function checkHealth() {
      var url = serverBase + '/availability';
      lastCheckAt = Date.now();
      var controller = typeof AbortController === 'function' ? new AbortController() : null;
      var timer = setTimeout(function () {
        if (controller) controller.abort();
      }, 5000);
      return fetch(url, { cache: 'no-store', credentials: 'omit', signal: controller && controller.signal })
        .then(function (r) {
          return r.ok;
        })
        .catch(function () {
          // Usually the server is down, but a host site's Content-Security-Policy can also block
          // this request; say so once so site owners can find it.
          if (!warnedUnreachable) {
            warnedUnreachable = true;
            if (window.console) console.warn('open-rust-chatbot: could not reach ' + url + '. If the chat never appears, allow it in your Content-Security-Policy connect-src.');
          }
          return false;
        })
        .then(function (ok) {
          clearTimeout(timer);
          return ok;
        });
    }

    function setAvailable(ok) {
      available = ok && !refused;
      // Never yank the panel away mid-conversation; the button hides once the panel is closed.
      root.hidden = !available && panel.dataset.open !== 'true';
    }

    function pauseInput(paused) {
      input.disabled = paused;
      sendBtn.disabled = paused || waiting;
      input.placeholder = paused ? 'Reconnecting…' : TEXT.placeholder;
    }

    function startOutage(message) {
      available = false;
      if (!outage) {
        outage = true;
        recoveryDelay = 0;
        say('error', message);
      }
      pauseInput(true);
      scheduleRecovery();
    }

    function stopOutage() {
      clearTimeout(recoveryTimer);
      outage = false;
      pauseInput(false);
    }

    function scheduleRecovery() {
      clearTimeout(recoveryTimer);
      recoveryDelay = Math.min(recoveryDelay ? recoveryDelay * 2 : 2000, 30000);
      recoveryTimer = setTimeout(function () {
        if (!outage) return;
        if (navigator.onLine === false || document.visibilityState === 'hidden') return scheduleRecovery();
        checkHealth().then(function (ok) {
          if (!outage) return;
          if (!ok) return scheduleRecovery();
          connect(); // "reconnected" is only announced once this socket actually opens
        });
      }, recoveryDelay);
    }

    function onConnected() {
      connectFailures = 0;
      refused = false;
      if (!outage) return;
      stopOutage();
      setAvailable(true);
      say('assistant', lostReply ? MSG_BACK : MSG_BACK_IDLE);
      lostReply = false;
    }

    // The socket closed before it ever opened. If the chat is down, that's an outage; if it's
    // up, the server refused us (not an allowed site, too busy, verification failed), so stop
    // retrying, say so once, and hide the button after repeated refusals.
    function onConnectFailed(wasWaiting, gen) {
      checkHealth().then(function (ok) {
        if (gen !== generation) return;
        if (!ok) {
          lostReply = lostReply || wasWaiting;
          if (panel.dataset.open === 'true' || wasWaiting) startOutage(wasWaiting ? MSG_LOST_REPLY : MSG_UNAVAILABLE);
          else setAvailable(false);
          return;
        }
        connectFailures++;
        var wasOutage = outage;
        if (outage) stopOutage();
        lostReply = false;
        if (wasWaiting || wasOutage || connectFailures === 1) say('error', wasWaiting ? MSG_NOT_SENT : MSG_CANT_CONNECT);
        if (connectFailures >= 3) {
          refused = true;
          refusedAt = Date.now();
          setAvailable(false);
        }
      });
    }

    // An open socket closed unexpectedly. A reply in flight is lost; otherwise it's only an
    // outage if the chat is really unavailable (servers close idle sockets harmlessly).
    function onDropped(wasWaiting, gen) {
      if (wasWaiting) {
        lostReply = true;
        startOutage(MSG_LOST_REPLY);
        return;
      }
      checkHealth().then(function (ok) {
        if (gen !== generation || ok) return;
        if (panel.dataset.open === 'true') startOutage(MSG_DROPPED);
        else setAvailable(false);
      });
    }

    function watchHealth(now) {
      clearTimeout(healthTimer);
      healthTimer = setTimeout(function () {
        if (document.visibilityState === 'hidden' || outage) return watchHealth();
        checkHealth().then(function (ok) {
          if (ok) {
            healthFailures = 0;
            // Refusals are often temporary (busy server, shared office IP): try again later.
            if (refused && Date.now() - refusedAt > REFUSAL_COOLDOWN_MS) {
              refused = false;
              connectFailures = 0;
            }
            setAvailable(true);
          } else if (++healthFailures >= 2) {
            // A live socket is better evidence than one failed check; it will report problems.
            var socketOpen = ws && ws.readyState === WebSocket.OPEN;
            if (panel.dataset.open !== 'true') setAvailable(false);
            else if (!socketOpen) startOutage(MSG_DROPPED);
          }
          watchHealth();
        });
      }, now ? 0 : available ? HEALTH_RECHECK_MS : HEALTH_RECHECK_DOWN_MS);
    }

    function connect() {
      if (connecting || (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING))) return;
      connecting = true;
      var gen = ++generation;
      wsUrl().then(
        function (url) {
          connecting = false;
          var socket = new WebSocket(url);
          ws = socket;
          var opened = false;
          socket.addEventListener('open', function () {
            opened = true;
            onConnected();
            // First message: continue this tab's conversation (single-use token) or start one.
            // The stored token is replaced when the server answers with a new one.
            socket.send(JSON.stringify({ type: 'resume', token: store.get('token') }));
            while (queue.length) socket.send(queue.shift());
          });
          socket.addEventListener('message', onFrame);
          socket.addEventListener('close', function () {
            if (ws !== socket) return;
            ws = null;
            // Never replay messages from a failed attempt on a later connection.
            queue = [];
            // Confirm buttons belong to the old session; the server has forgotten those cards.
            messages.querySelectorAll('.orc-card-action').forEach(function (b) {
              b.disabled = true;
            });
            var wasWaiting = waiting;
            if (waiting) setWaiting(false);
            current = null;
            if (closingOnPurpose === socket) {
              closingOnPurpose = null;
              return;
            }
            if (opened) onDropped(wasWaiting, gen);
            else onConnectFailed(wasWaiting, gen);
          });
        },
        function () {
          // e.g. a Turnstile token callback failed
          connecting = false;
          queue = [];
          var wasWaiting = waiting;
          setWaiting(false);
          onConnectFailed(wasWaiting, gen);
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

    function open(opts) {
      panel.dataset.open = 'true';
      store.set('open', '1');
      bubble.setAttribute('aria-expanded', 'true');
      bubble.setAttribute('aria-label', 'Close chat');
      if (!greeted) {
        greeted = true;
        greetingNode = append('assistant', TEXT.greeting);
      }
      connect();
      if (!opts || opts.focus !== false) input.focus();
    }

    function close() {
      panel.dataset.open = 'false';
      store.set('open', null);
      bubble.setAttribute('aria-expanded', 'false');
      bubble.setAttribute('aria-label', 'Open chat');
      if (!available) {
        root.hidden = true;
        return;
      }
      bubble.focus();
    }

    bubble.setAttribute('aria-expanded', 'false');
    bubble.addEventListener('click', function () {
      if (panel.dataset.open === 'true') close();
      else open();
    });
    closeBtn.addEventListener('click', close);
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape' && panel.dataset.open === 'true') close();
    });
    window.addEventListener('offline', function () {
      if (panel.dataset.open !== 'true' && !waiting) return setAvailable(false);
      var wasWaiting = waiting;
      if (waiting) {
        setWaiting(false);
        current = null;
      }
      lostReply = lostReply || wasWaiting;
      if (ws) {
        closingOnPurpose = ws;
        ws.close();
      }
      startOutage(wasWaiting ? MSG_LOST_REPLY : MSG_OFFLINE);
    });
    window.addEventListener('online', function () {
      if (outage) {
        recoveryDelay = 0;
        scheduleRecovery();
      } else {
        watchHealth(true);
      }
    });
    document.addEventListener('visibilitychange', function () {
      if (document.visibilityState === 'visible' && !outage && Date.now() - lastCheckAt > 15000) watchHealth(true);
    });

    // Continuing from a previous page: show the conversation and reopen if it was open.
    var restoredFromStorage = restoreTranscript();
    if (restoredFromStorage) greeted = true;
    // Leaving mid-reply keeps what has arrived so far.
    window.addEventListener('pagehide', saveTranscript);
    var reopen = store.get('open') === '1';

    // Server-provided texts arrived: update what is already on screen.
    settings.then(function (remote) {
      var oldGreeting = TEXT.greeting;
      applySettings(remote);
      titleNode.textContent = TEXT.title;
      subtitleNode.textContent = TEXT.subtitle;
      panel.setAttribute('aria-label', TEXT.title);
      if (!input.disabled) input.placeholder = TEXT.placeholder;
      input.setAttribute('aria-label', TEXT.placeholder);
      showPrivacyLink();
      showAvatar();
      if (greetingNode && greetingNode.textContent === oldGreeting && messages.lastChild === greetingNode) {
        greetingNode.textContent = TEXT.greeting;
        saveTranscript();
      }
    });

    checkHealth().then(function (ok) {
      setAvailable(ok);
      // Reopen quietly on larger screens only: on phones the panel would cover the page the
      // visitor just went to, so they get the button back instead.
      if (ok && reopen && !window.matchMedia('(max-width: 480px)').matches) open({ focus: false });
      watchHealth();
    });
    inputRow.addEventListener('submit', function (e) {
      e.preventDefault();
      var text = input.value.trim();
      if (!text || waiting || outage) return;
      append('user', text);
      saveTranscript();
      input.value = '';
      setWaiting(true);
      send(text);
    });
  }

  applySettings(null);
  var settings = loadSettings();
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', mount, { once: true });
  else mount();
})();
