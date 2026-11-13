/*
 * The Family page, on the dashboard's own shell: a glass sidebar of sections, a page header
 * with its icon tile, and glass cards. Everyone sees "You" (overview, devices, requests, saved
 * songs, their library, audio quality and account); people who manage the family also see
 * "Family" (members, requests to approve, all devices, family playlists and activity).
 * Each person has one login (server, username and the password they chose) that works in any
 * app; devices are tracked as they sign in, never set up here. Everything goes through
 * /api/family; the page keeps no secrets of its own.
 */
(function () {
  'use strict';

  var F = window.OctoFamily;
  var el = F.el;
  var api = F.api;
  var app = document.getElementById('app');
  var session = null;

  // ── Plain words ──────────────────────────────────────────────────────────────────────
  var ROLE_WORDS = {
    Owner: 'Owner',
    CoAdmin: 'Co-admin',
    Member: 'Member',
    Listener: 'Listener',
    Kid: 'Kid',
    Unmanaged: 'Not in the family'
  };

  var PRESETS = {
    CoAdmin: 'Can manage the family and approve requests, and adds songs straight to their own library.',
    Member: 'Adds songs straight to their own library, with up to 50 GB of space.',
    Listener: 'Saves songs from outside the library and asks for copies, up to 10 a week.',
    Kid: 'Hears clean songs only, and every request needs your approval.'
  };

  // Every preset's abilities, as the server's presets set them.
  var BASE = {
    addToLibrary: 'Direct', musicGoesTo: 'Own', requestQuality: 'Best', autoApprove: true, weeklyRequestLimit: 0, streamCap: 0,
    awayCap: 0, away: true, devicesAtOnce: 0, downloadFiles: true, offlineCopies: true, share: true,
    importPlaylists: true, cleanOnly: false, familyPlaylistsEdit: true, manageFamily: true, approveRequests: true,
    storageLimitGb: 0, instantFromFamily: true
  };
  var PRESET_ABILITIES = {
    CoAdmin: Object.assign({}, BASE),
    Member: Object.assign({}, BASE, { manageFamily: false, approveRequests: false, storageLimitGb: 50 }),
    Listener: Object.assign({}, BASE, {
      addToLibrary: 'Request', autoApprove: false, weeklyRequestLimit: 10, downloadFiles: false, share: false,
      manageFamily: false, approveRequests: false, storageLimitGb: 10
    })
  };
  PRESET_ABILITIES.Kid = Object.assign({}, PRESET_ABILITIES.Listener, {
    requestQuality: 'Mp3', weeklyRequestLimit: 5, cleanOnly: true, familyPlaylistsEdit: false, storageLimitGb: 5,
    instantFromFamily: false
  });

  // The abilities a manager edits, grouped as the edit sheet shows them, each with one line on
  // what it does.
  var ABILITY_GROUPS = [
    { title: 'Listening', fields: [
      { key: 'away', title: 'Listen away from home', help: 'Off keeps their apps and links to your home network.', kind: 'switch' },
      { key: 'streamCap', title: 'Highest quality at home, in kbps', help: '0 means the original file. Their own choice can only go lower.', kind: 'number' },
      { key: 'awayCap', title: 'Highest quality away from home, in kbps', help: '0 means the same as at home.', kind: 'number' },
      { key: 'devicesAtOnce', title: 'Devices playing at once', help: '0 means no limit. The newest device plays.', kind: 'number' },
      { key: 'offlineCopies', title: 'Keep songs for offline', help: 'In the Octo apps, for planes and bad signal.', kind: 'switch' },
      { key: 'downloadFiles', title: 'Download song files', help: 'Off means songs stream only.', kind: 'switch' },
      { key: 'cleanOnly', title: 'Clean songs only', help: 'Songs marked explicit do not play for them.', kind: 'switch' }
    ] },
    { title: 'Adding music', fields: [
      { key: 'addToLibrary', title: 'A heart on a song from outside the library', help: 'What happens when they heart a song Octo found elsewhere.', kind: 'choice',
        options: [['Direct', 'Add it to their library'], ['Request', 'Save it; they can ask for a copy'], ['SaveOnly', 'Save it only']] },
      { key: 'musicGoesTo', title: 'Music goes to', help: 'Their own library, or the shared library everyone in the family plays.', kind: 'choice',
        options: [['Own', 'Their own library'], ['Shared', 'Shared library']] },
      { key: 'requestQuality', title: 'Best copy they can ask for', help: 'The highest quality their requests fetch.', kind: 'choice',
        options: [['Best', 'Best there is'], ['Flac', 'FLAC'], ['Mp3', 'MP3']] },
      { key: 'autoApprove', title: 'Approve their requests at once', help: 'Off means each request waits for you.', kind: 'switch' },
      { key: 'instantFromFamily', title: 'Add songs the family already has at once', help: 'No download and no extra space on disk.', kind: 'switch' },
      { key: 'importPlaylists', title: 'Import playlists', help: 'From Spotify and other services.', kind: 'switch' }
    ] },
    { title: 'Sharing', fields: [
      { key: 'share', title: 'Make share links', help: 'Links people outside the family can open.', kind: 'switch' },
      { key: 'familyPlaylistsEdit', title: 'Add to family playlists', help: 'Off means family playlists are to listen to.', kind: 'switch' }
    ] },
    { title: 'Limits', fields: [
      { key: 'weeklyRequestLimit', title: 'Requests a week', help: '0 means no limit.', kind: 'number' },
      { key: 'storageLimitGb', title: 'Space for their own library, in GB', help: '0 means no limit. When it is full, adding a song becomes a request.', kind: 'number' }
    ] },
    { title: 'Managing', owner: true, fields: [
      { key: 'manageFamily', title: 'Manage the family', help: 'Add and edit members, and see everyone’s devices.', kind: 'switch' },
      { key: 'approveRequests', title: 'Approve requests', help: 'Decide on everyone’s requests.', kind: 'switch' }
    ] }
  ];
  var ABILITY_FIELDS = ABILITY_GROUPS.reduce(function (all, g) { return all.concat(g.fields); }, []);

  var QUALITIES = [
    ['Original', 'Original'],
    ['High', 'High'],
    ['Standard', 'Standard'],
    ['DataSaver', 'Data saver']
  ];
  var QUALITY_HELP = {
    Original: 'the file as it is.',
    High: 'about 256 kbps.',
    Standard: 'about 160 kbps.',
    DataSaver: 'about 96 kbps, for slow or metered connections.'
  };

  var STATE_WORDS = {
    Pending: ['Waiting', 'warn'],
    Approved: ['Approved', 'accent'],
    Declined: ['Declined', ''],
    Cancelled: ['Canceled', ''],
    Done: ['In the library', 'good'],
    Failed: ['Failed', 'bad']
  };

  function qualityWord(q) {
    return q === 'Flac' ? 'FLAC' : q === 'Mp3' ? 'MP3' : 'Best quality';
  }

  function badge(text, tone) {
    return el('span', { class: 'fam-badge' + (tone ? ' ' + tone : '') }, text);
  }

  function stateBadge(state) {
    var words = STATE_WORDS[state] || [state, ''];
    return badge(words[0], words[1]);
  }

  function placeBadge(place) {
    return place === 'Away' ? badge('Away', 'warn') : badge('Home', 'good');
  }

  function meter(used, limitBytes, words) {
    var pct = limitBytes > 0 ? Math.min(100, Math.round((used / limitBytes) * 100)) : 0;
    var full = limitBytes > 0 && used >= limitBytes;
    return el('div', { class: 'fam-meter' },
      el('div', { class: 'fam-meter-line' },
        el('span', null, words),
        el('span', { class: 'fam-meter-n' }, limitBytes > 0 ? F.bytes(used) + ' of ' + F.bytes(limitBytes) : F.bytes(used) + ', no limit')),
      el('div', { class: 'fam-bar' + (full ? ' full' : ''), role: 'progressbar', 'aria-label': words, 'aria-valuemin': 0, 'aria-valuemax': 100, 'aria-valuenow': pct },
        el('span', { style: 'width:' + (limitBytes > 0 ? pct : 0) + '%' })));
  }

  function countMeter(used, limit, words) {
    var pct = limit > 0 ? Math.min(100, Math.round((used / limit) * 100)) : 0;
    return el('div', { class: 'fam-meter' },
      el('div', { class: 'fam-meter-line' },
        el('span', null, words),
        el('span', { class: 'fam-meter-n' }, limit > 0 ? used + ' of ' + limit : used + ', no limit')),
      el('div', { class: 'fam-bar' + (limit > 0 && used >= limit ? ' full' : ''), role: 'progressbar', 'aria-label': words, 'aria-valuemin': 0, 'aria-valuemax': 100, 'aria-valuenow': pct },
        el('span', { style: 'width:' + pct + '%' })));
  }

  // A member's abilities in plain words.
  function planLines(a) {
    var lines = [];
    var add = function (on, text) { lines.push(el('li', { class: on ? '' : 'off' }, text)); };
    if (a.addToLibrary === 'Direct') add(true, 'A heart on a song from outside the library adds it to your library.');
    else if (a.addToLibrary === 'Request') add(true, 'A heart on a song from outside the library saves it, and you can ask for a copy.');
    else add(true, 'A heart on a song from outside the library saves it to play any time.');
    if (a.addToLibrary !== 'SaveOnly') {
      add(true, 'You can ask for ' + (a.requestQuality === 'Best' ? 'the best copy there is' : a.requestQuality === 'Flac' ? 'FLAC copies' : 'MP3 copies') +
        (a.autoApprove ? ', and requests are approved at once.' : '; each request waits for approval.'));
    }
    if (a.streamCap > 0) add(true, 'Songs stream at up to ' + a.streamCap + ' kbps at home.');
    if (!a.away) add(false, 'Listening away from home is off.');
    else if (a.awayCap > 0) add(true, 'Away from home, songs stream at up to ' + a.awayCap + ' kbps.');
    else add(true, 'You can listen at home and away.');
    if (a.devicesAtOnce > 0) add(true, a.devicesAtOnce === 1 ? 'One device plays at a time.' : a.devicesAtOnce + ' devices can play at once.');
    add(a.offlineCopies, a.offlineCopies ? 'The Octo apps can keep songs for offline.' : 'Offline copies are off.');
    add(a.downloadFiles, a.downloadFiles ? 'You can download song files.' : 'Song files stream only; downloads are off.');
    add(a.share, a.share ? 'You can make share links.' : 'Share links are off.');
    add(a.familyPlaylistsEdit, a.familyPlaylistsEdit ? 'You can add to family playlists.' : 'Family playlists are to listen to.');
    if (a.cleanOnly) add(true, 'Only clean songs play.');
    return el('ul', { class: 'fam-lines' }, lines);
  }

  // ── Building blocks on the dashboard's classes ───────────────────────────────────────
  function pageHeader(icon, title, desc, actions) {
    return el('header', { class: 'page-header' },
      el('span', { class: 'page-icon', 'aria-hidden': 'true' }, F.icon(icon)),
      el('div', null, el('h1', { tabindex: '-1' }, title), desc ? el('p', null, desc) : null),
      actions ? el('div', { class: 'fam-head-actions' }, actions) : null);
  }

  // A glass card with an optional title, a line under it and something on the right.
  function card(title, desc, side) {
    var node = el('section', { class: 'info-card fam-card' });
    if (title || side) {
      node.appendChild(el('div', { class: 'fam-card-head' },
        el('div', null, title ? el('h2', { class: 'info-card-title' }, title) : null, desc ? el('p', { class: 'fam-card-desc' }, desc) : null),
        side || null));
    }
    for (var i = 3; i < arguments.length; i++) F.append(node, arguments[i]);
    return node;
  }

  // A glass card of rows, as the dashboard's settings cards are.
  function rowsCard(rows) {
    var node = el('div', { class: 'set-card fam-rows' });
    F.append(node, rows);
    return node;
  }

  // An empty list: what would be here, and the thing to do about it.
  function empty(icon, text, action) {
    return el('div', { class: 'fam-empty' },
      el('span', { class: 'fam-empty-ico', 'aria-hidden': 'true' }, F.icon(icon)),
      el('p', null, text),
      action || null);
  }

  function switchControl(checked, label, onchange) {
    var input = el('input', { type: 'checkbox', checked: checked, 'aria-label': label, onchange: onchange });
    return { input: input, node: el('label', { class: 'switch' }, input, el('span', { class: 'sw-track' }), el('span', { class: 'sw-thumb' })) };
  }

  function segmented(options, value, onPick, label) {
    var group = el('div', { class: 'seg fam-seg', role: 'group', 'aria-label': label });
    options.forEach(function (option) {
      group.appendChild(el('button', {
        type: 'button',
        class: option[0] === value ? 'active' : '',
        'aria-pressed': option[0] === value ? 'true' : 'false',
        onclick: function () {
          Array.prototype.forEach.call(group.children, function (b) { b.setAttribute('aria-pressed', 'false'); b.classList.remove('active'); });
          this.setAttribute('aria-pressed', 'true');
          this.classList.add('active');
          onPick(option[0]);
        }
      }, option[1]));
    });
    return group;
  }

  function setRow(title, desc, control, stack) {
    return el('div', { class: 'set-row' + (stack ? ' stack' : '') },
      el('div', { class: 'set-info' }, el('span', { class: 'set-info-t' }, title), desc ? el('span', { class: 'set-info-d' }, desc) : null),
      control ? el('div', { class: 'set-ctrl' }, control) : null);
  }

  function failed(main, reply) {
    F.clear(main).appendChild(el('p', { class: 'notice notice-error', role: 'alert' }, F.errorOf(reply)));
  }

  function confirmIn(words) {
    return window.confirm(words);
  }

  // ── Navigation ───────────────────────────────────────────────────────────────────────
  var VIEWS_INFO = {
    overview: ['Overview', 'pulse'],
    listening: ['Listening', 'headphones'],
    devices: ['Devices', 'phone'],
    requests: ['Requests', 'tray'],
    saved: ['Saved', 'heart'],
    library: ['My library', 'folder'],
    import: ['Import', 'playlist'],
    ready: ['Ready to play', 'stack'],
    quality: ['Audio quality', 'sliders'],
    account: ['Account', 'key'],
    members: ['Members', 'users'],
    inbox: ['Requests to approve', 'checkCircle'],
    'all-devices': ['All devices', 'plugs'],
    playlists: ['Family playlists', 'playlist'],
    activity: ['Activity', 'drives'],
    'everyone-listening': ['Everyone\u2019s listening', 'headphones']
  };

  function navFor(s) {
    var you = s.managed ? ['overview', 'listening', 'devices', 'requests', 'saved', 'library', 'ready', 'quality', 'account'] : ['overview', 'devices', 'quality'];
    if (s.managed && !s.familyEnabled) you.splice(you.indexOf('listening'), 1);
    if (s.managed && !s.familyEnabled) you.splice(you.indexOf('ready'), 1);
    if (s.managed && s.familyEnabled && s.importPlaylists) you.splice(you.indexOf('library') + 1, 0, 'import');
    var family = [];
    if (s.familyEnabled) {
      if (s.manager) family = ['members', 'inbox', 'all-devices', 'playlists', 'activity', 'everyone-listening'];
      else if (s.approver) family = ['inbox', 'playlists'];
      else if (s.role !== 'Unmanaged') family = ['playlists'];
    } else if (s.owner) {
      family = ['members'];
    }
    return { you: you, family: family };
  }

  function defaultView(s) {
    if (!s.familyEnabled) return s.owner ? 'members' : 'devices';
    return 'overview';
  }

  // Old addresses from before the sidebar still land somewhere sensible.
  var OLD_VIEWS = { me: 'overview', family: 'members', load: 'activity' };

  function currentView() {
    var match = /^#\/([a-z-]+)/.exec(location.hash);
    var view = match ? match[1] : defaultView(session);
    return OLD_VIEWS[view] || view;
  }

  function navItem(view, current) {
    var info = VIEWS_INFO[view];
    var count = view === 'inbox' && session.pending ? el('span', { class: 'sidebar-nav-badge bad' }, String(session.pending)) : null;
    return el('a', {
      class: 'sidebar-nav-item' + (view === current ? ' active' : ''),
      href: '#/' + view,
      'aria-current': view === current ? 'page' : null,
      onclick: closeDrawer
    }, F.icon(info[1]), el('span', null, info[0]), count, count ? el('span', { class: 'visually-hidden' }, ' waiting') : null);
  }

  function openDrawer() {
    document.body.classList.add('fam-drawer-open');
    var current = document.querySelector('.sidebar .sidebar-nav-item.active') || document.querySelector('.sidebar .sidebar-nav-item');
    if (current) current.focus();
  }

  function closeDrawer() {
    document.body.classList.remove('fam-drawer-open');
  }

  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape' && document.body.classList.contains('fam-drawer-open')) {
      closeDrawer();
      var menu = document.getElementById('fam-menu');
      if (menu) menu.focus();
    }
  });

  function frame(view) {
    var nav = navFor(session);
    var sections = [el('div', { class: 'sidebar-nav-section' },
      el('div', { class: 'sidebar-nav-label' }, 'You'),
      nav.you.map(function (v) { return navItem(v, view); }))];
    if (nav.family.length) {
      sections.push(el('div', { class: 'sidebar-nav-section' },
        el('div', { class: 'sidebar-nav-label' }, 'Family'),
        nav.family.map(function (v) { return navItem(v, view); })));
    }
    if (session.owner) {
      sections.push(el('div', { class: 'sidebar-nav-section' },
        el('div', { class: 'sidebar-nav-label' }, 'Server'),
        el('a', { class: 'sidebar-nav-item', href: '/admin/' }, F.icon('out'), el('span', null, 'Server dashboard'))));
    }
    var main = el('main', { class: 'content', id: 'view' }, el('p', { class: 'state' }, 'Loading'));
    var sidebar = el('aside', { class: 'sidebar', id: 'fam-sidebar', 'aria-label': 'Family page' },
      el('div', { class: 'sidebar-brand' },
        el('img', { src: '/Assets/octo_logo.png', alt: '', class: 'sidebar-logo' }),
        el('div', { class: 'sidebar-brand-text' },
          el('div', { class: 'sidebar-brand-name' }, 'Octo Family'),
          el('div', { class: 'sidebar-brand-tag' }, session.displayName + ' · ' + (ROLE_WORDS[session.role] || session.role)))),
      el('nav', { class: 'sidebar-nav', 'aria-label': 'Sections' }, sections),
      el('div', { class: 'sidebar-footer' },
        el('button', { class: 'btn btn-ghost btn-block', type: 'button', onclick: signOut }, F.icon('signin'), el('span', null, 'Sign out'))));
    var top = el('header', { class: 'fam-topbar' },
      el('button', { class: 'btn btn-ghost btn-icon', id: 'fam-menu', type: 'button', 'aria-label': 'Open the menu', 'aria-controls': 'fam-sidebar', onclick: openDrawer }, F.icon('menu')),
      el('img', { src: '/Assets/octo_logo.png', alt: '', class: 'fam-topbar-logo' }),
      el('span', { class: 'fam-topbar-name' }, 'Octo Family'));
    F.clear(app).appendChild(el('div', { class: 'app fam-app' },
      top,
      el('div', { class: 'fam-scrim', onclick: closeDrawer, 'aria-hidden': 'true' }),
      sidebar,
      main));
    return main;
  }

  async function signOut() {
    await api('POST', '/signout', {});
    session = null;
    location.hash = '';
    renderSignIn();
  }

  // ── Sign in ──────────────────────────────────────────────────────────────────────────
  function renderSignIn(message) {
    var user = el('input', { class: 'set-input', id: 'username', name: 'username', autocomplete: 'username', autocapitalize: 'none', spellcheck: 'false', required: true });
    var pass = el('input', { class: 'set-input', id: 'password', name: 'password', type: 'password', autocomplete: 'current-password', required: true });
    var error = el('p', { class: 'field-error', role: 'alert' }, message || '');
    var button = el('button', { class: 'btn btn-primary btn-block fam-big', type: 'submit' }, 'Sign in');
    var form = el('form', { class: 'fam-form', onsubmit: async function (e) {
      e.preventDefault();
      button.disabled = true;
      error.textContent = '';
      var reply = await api('POST', '/auth', { username: user.value.trim(), password: pass.value });
      button.disabled = false;
      if (!reply.ok) {
        error.textContent = F.errorOf(reply);
        pass.value = '';
        pass.focus();
        return;
      }
      start();
    } },
    el('label', { class: 'fam-label', for: 'username' }, 'Username', user),
    el('label', { class: 'fam-label', for: 'password' }, 'Password', pass),
    error,
    button);
    F.clear(app).appendChild(el('div', { class: 'fam-center' },
      el('div', { class: 'fam-solo' },
        el('div', { class: 'sidebar-brand fam-solo-brand' },
          el('img', { src: '/Assets/octo_logo.png', alt: '', class: 'sidebar-logo' }),
          el('div', { class: 'sidebar-brand-text' }, el('div', { class: 'sidebar-brand-name' }, 'Octo Family'), el('div', { class: 'sidebar-brand-tag' }, location.host))),
        el('section', { class: 'info-card fam-card' },
          el('h1', { class: 'fam-solo-title' }, 'Sign in'),
          el('p', { class: 'fam-card-desc' }, 'Family members use the password they set from their invite link. The owner uses their Navidrome sign-in.'),
          form,
          el('p', { class: 'fam-help fam-gap' }, 'Forgot your password? Ask whoever runs this server for a new sign-in link.')))));
    user.focus();
  }

  // ── You: Overview ────────────────────────────────────────────────────────────────────
  function nextStep(icon, title, desc, action) {
    return el('section', { class: 'callout fam-next' },
      el('span', { class: 'page-icon fam-next-ico', 'aria-hidden': 'true' }, F.icon(icon)),
      el('div', { class: 'fam-next-text' }, el('div', { class: 'callout-title' }, title), el('p', null, desc)),
      el('div', { class: 'fam-next-act' }, action));
  }

  async function viewOverview(main) {
    if (!session.managed) return session.manager ? viewFamilyOverview(main) : viewGuestOverview(main);
    var replies = await Promise.all([api('GET', '/me'), api('GET', '/me/devices'), api('GET', '/me/requests'), api('GET', '/me/login')]);
    if (!replies[0].ok) return failed(main, replies[0]);
    var me = replies[0].data;
    var a = me.abilities;
    var devices = replies[1].ok ? replies[1].data : [];
    var requests = replies[2].ok ? replies[2].data : [];
    var login = replies[3].ok ? replies[3].data : null;
    // Shared music never counts against a limit on their own folder.
    var limit = a.musicGoesTo !== 'Shared' && a.storageLimitGb > 0 ? a.storageLimitGb * 1e9 : 0;
    var playing = devices.filter(function (d) { return d.playing; });

    // One next step, the most useful first.
    var ready = requests.filter(function (r) { return r.state === 'Done'; })[0];
    var waiting = requests.filter(function (r) { return r.state === 'Pending'; });
    var next;
    if (!devices.length) {
      next = nextStep('phone', 'Sign in on your first app', 'Use your login below in Octo or any Subsonic app. Each device shows up here once it signs in.',
        el('a', { class: 'btn', href: '#/account' }, F.icon('key'), 'Your login'));
    } else if (ready) {
      next = nextStep('checkCircle', 'Your request for ' + (ready.title || 'a song') + ' is ready', 'It is in your library now, on every device.',
        el('a', { class: 'btn', href: '#/requests' }, 'See your requests'));
    } else if (waiting.length) {
      next = nextStep('clock', waiting.length === 1 ? 'One request is waiting for approval' : waiting.length + ' requests are waiting for approval', 'You hear back when they are decided.',
        el('a', { class: 'btn', href: '#/requests' }, 'See your requests'));
    } else if (limit > 0 && me.storageUsedBytes >= limit) {
      next = nextStep('warning', 'Your library is full', 'New songs become requests until there is room. Remove songs you no longer play.',
        el('a', { class: 'btn', href: '#/library' }, 'Open my library'));
    } else {
      next = nextStep('heart', 'You are all set', 'Heart songs in your app, and they show up here.',
        el('a', { class: 'btn', href: '#/saved' }, 'Open saved songs'));
    }

    var counts = {};
    requests.forEach(function (r) { counts[r.state] = (counts[r.state] || 0) + 1; });
    var statusBadges = Object.keys(counts).map(function (state) {
      var words = STATE_WORDS[state] || [state, ''];
      return badge(counts[state] + ' ' + words[0].toLowerCase(), words[1]);
    });

    var nowPlaying = playing.length
      ? el('ul', { class: 'fam-list' }, playing.map(function (d) {
        return el('li', { class: 'fam-row' },
          el('span', { class: 'fam-row-ico', 'aria-hidden': 'true' }, F.icon('radio')),
          el('div', { class: 'fam-row-main' },
            el('div', { class: 'fam-row-title' }, d.playing.title),
            el('div', { class: 'fam-row-meta' }, [d.playing.artist, d.name || d.app].filter(Boolean).join(' · '))),
          placeBadge(d.place));
      }))
      : el('p', { class: 'fam-quiet' }, 'Nothing is playing right now.');

    F.clear(main);
    F.append(main, [
      pageHeader('pulse', 'Hi, ' + me.displayName, 'Your plan is ' + (ROLE_WORDS[me.role] || me.role) + '. Your space, your requests and what is playing, at a glance.'),
      next,
      el('div', { class: 'fam-grid three' },
        card('Your space', 'Songs added for you.', null, meter(me.storageUsedBytes, limit, 'Your library'),
          limit > 0 && me.storageUsedBytes >= limit ? el('p', { class: 'fam-help fam-gap' }, 'Full, so new songs become requests.') : null),
        card('Requests this week', a.addToLibrary === 'SaveOnly' ? 'Your plan saves songs without asking for copies.' : 'Copies you asked for.', null,
          a.addToLibrary !== 'SaveOnly' ? countMeter(me.requestsThisWeek, a.weeklyRequestLimit, 'This week') : null,
          statusBadges.length ? el('div', { class: 'fam-badges fam-gap' }, statusBadges) : null),
        card('Playing now', devices.length + (devices.length === 1 ? ' device' : ' devices') + ' signed in.', null, nowPlaying)),
      el('div', { class: 'fam-grid two' },
        login ? loginCard(login) : null,
        card('What your plan allows', 'Set by whoever runs the family.', null, planLines(a)))
    ]);
  }

  // The owner's (and any manager outside the family's) overview: who is playing, what is
  // waiting, what is new, everyone's space, the outside address, and one next step.
  async function viewFamilyOverview(main) {
    var replies = await Promise.all([api('GET', '/members'), api('GET', '/devices'), api('GET', '/requests?state=Pending'), api('GET', '/load')]);
    if (!replies[0].ok) return failed(main, replies[0]);
    var members = replies[0].data;
    var devices = replies[1].ok ? replies[1].data : [];
    var waiting = replies[2].ok ? replies[2].data : [];
    var streams = replies[3].ok ? replies[3].data.streams : [];
    var byUser = {};
    members.forEach(function (m) { byUser[m.username.toLowerCase()] = m; });
    var nameOf = function (user) {
      var m = byUser[String(user || '').toLowerCase()];
      return m ? m.displayName : user;
    };
    var weekAgo = Date.now() - 7 * 24 * 3600 * 1000;
    var fresh = devices.filter(function (d) { return byUser[d.username.toLowerCase()] && Date.parse(d.firstSeen) > weekAgo; })
      .sort(function (a, b) { return Date.parse(b.firstSeen) - Date.parse(a.firstSeen); });
    var notJoined = members.filter(function (m) { return !m.joined; });
    var needsPassword = members.filter(function (m) { return m.needsPassword; });

    var approveAll = el('button', { class: 'btn btn-sm btn-primary', type: 'button', onclick: async function () {
      if (!confirmIn('Approve all ' + waiting.length + (waiting.length === 1 ? ' request' : ' requests') + '? Each one starts fetching its song.')) return;
      approveAll.disabled = true;
      var failures = 0;
      for (var i = 0; i < waiting.length; i++) {
        var r = await api('POST', '/requests/' + encodeURIComponent(waiting[i].id) + '/approve', { note: '' });
        if (!r.ok) failures++;
      }
      session.pending = failures;
      F.toast(failures ? failures + ' could not be approved. See Requests to approve.' : 'All approved', failures > 0);
      route();
    } }, F.icon('check'), 'Approve all');

    var next;
    if (!members.length) {
      next = nextStep('users', 'Add your first member', 'They get a link to set their password, then add their own devices.',
        el('button', { class: 'btn btn-primary', type: 'button', onclick: addMemberSheet }, F.icon('plus'), 'Add member'));
    } else if (needsPassword.length) {
      next = nextStep('key', needsPassword[0].displayName + ' needs a new password', 'Their password no longer works, so their apps cannot sign in. Reset it to send them a link to choose a new one.',
        el('button', { class: 'btn btn-primary', type: 'button', onclick: function () { resetPassword(needsPassword[0]); } }, F.icon('key'), 'Reset password'));
    } else if (waiting.length) {
      next = nextStep('checkCircle', waiting.length === 1 ? 'One request is waiting for you' : waiting.length + ' requests are waiting for you',
        'Approve them all here, or one at a time with a note.', el('a', { class: 'btn', href: '#/inbox' }, 'Review requests'));
    } else if (!session.publicUrl) {
      next = nextStep('globe', 'Set your outside address', 'Then invite links and everyone’s apps work away from home too.',
        el('a', { class: 'btn', href: STATUS_ADDRESS }, F.icon('out'), 'Set it on the Status page'));
    } else if (notJoined.length) {
      next = nextStep('send', notJoined[0].displayName + ' has not joined yet', 'Send their link again, or show them the QR code.',
        el('button', { class: 'btn', type: 'button', onclick: function () { inviteDialog(notJoined[0], null); } }, 'Send their link'));
    } else {
      next = nextStep('heart', 'Everything is running', 'Everyone has joined and nothing is waiting for you.',
        el('button', { class: 'btn', type: 'button', onclick: addMemberSheet }, F.icon('plus'), 'Add member'));
    }

    var playing = streams.length
      ? el('ul', { class: 'fam-list' }, streams.map(function (s) {
        return el('li', { class: 'fam-row' },
          F.avatar(nameOf(s.username)),
          el('div', { class: 'fam-row-main' },
            el('div', { class: 'fam-row-title' }, nameOf(s.username)),
            el('div', { class: 'fam-row-meta' }, [s.songTitle || 'A song', s.device].filter(Boolean).join(' · '))),
          placeBadge(s.place));
      }))
      : empty('radio', 'Nobody is playing right now.');

    var requestList = waiting.length
      ? el('div', null,
        el('ul', { class: 'fam-list' }, waiting.slice(0, 4).map(function (r) {
          return el('li', { class: 'fam-row' },
            cover(r.coverArt, r.title),
            el('div', { class: 'fam-row-main' },
              el('div', { class: 'fam-row-title' }, r.title || r.target),
              el('div', { class: 'fam-row-meta' }, (r.displayName || nameOf(r.username)) + ' · ' + F.ago(r.created))));
        })),
        el('div', { class: 'fam-actions' }, approveAll, el('a', { class: 'btn btn-sm btn-ghost', href: '#/inbox' }, waiting.length > 4 ? 'See all ' + waiting.length : 'Review one by one')))
      : empty('checkCircle', 'Nothing is waiting for you.');

    var newDevices = fresh.length
      ? el('ul', { class: 'fam-list' }, fresh.slice(0, 5).map(function (d) {
        return el('li', { class: 'fam-row' },
          el('span', { class: 'fam-row-ico', 'aria-hidden': 'true' }, F.icon('phone')),
          el('div', { class: 'fam-row-main' },
            el('div', { class: 'fam-row-title' }, deviceTitle(d)),
            el('div', { class: 'fam-row-meta' }, nameOf(d.username) + ' · first seen ' + F.ago(d.firstSeen).toLowerCase())));
      }))
      : empty('phone', 'No new devices this week. Devices show up as they sign in.');

    var storage = members.length
      ? el('div', { class: 'fam-storage' }, members.map(function (m) {
        return meter(m.storageUsedBytes, m.storageLimitGb > 0 ? m.storageLimitGb * 1e9 : 0, m.displayName);
      }))
      : empty('folder', 'Each member’s own library shows here.');

    var address = session.publicUrl
      ? card('Outside address', null, null,
        el('p', { class: 'fam-where-uses' }, F.icon('globe'), el('span', null, 'Links work anywhere at ', el('code', null, session.publicUrl))),
        el('div', { class: 'fam-actions' }, el('a', { class: 'btn btn-sm btn-ghost', href: STATUS_ADDRESS }, F.icon('out'), 'Change it on the Status page')))
      : addressNotice(function () { route(); });

    F.clear(main);
    F.append(main, [
      pageHeader('pulse', 'Overview', 'Your family at a glance: who is playing, what is waiting for you, and what is new.',
        el('button', { class: 'btn btn-primary', type: 'button', onclick: addMemberSheet }, F.icon('plus'), 'Add member')),
      next,
      el('div', { class: 'fam-grid two' },
        card('Playing now', streams.length ? streams.length + (streams.length === 1 ? ' stream' : ' streams') + ' right now.' : 'By member, with their device.', null, playing),
        card('Requests waiting', waiting.length ? 'Copies family members asked for.' : 'Copies family members ask for wait here.', null, requestList)),
      el('div', { class: 'fam-grid two' },
        card('New devices', 'First seen in the last 7 days.', null, newDevices),
        card('Storage', 'Each member’s own library.', null, storage)),
      address
    ]);
  }

  // Someone outside the family: the apps Octo has seen them use, and their audio quality.
  async function viewGuestOverview(main) {
    var reply = await api('GET', '/me/devices');
    var devices = reply.ok ? reply.data : [];
    F.clear(main);
    F.append(main, [
      pageHeader('pulse', 'Hi, ' + session.displayName, 'Your account is not in the family, but your audio quality is yours to choose.'),
      nextStep('sliders', 'Choose your audio quality', 'What Octo sends your devices, at home and away.', el('a', { class: 'btn', href: '#/quality' }, 'Open audio quality')),
      card('Your devices', devices.length + (devices.length === 1 ? ' app seen.' : ' apps seen.'), null,
        devices.length ? el('ul', { class: 'fam-list' }, devices.slice(0, 5).map(function (d) { return deviceRow(d, {}); }))
          : empty('phone', 'Octo has not seen a device of yours yet.'))
    ]);
  }

  // ── You: Devices ─────────────────────────────────────────────────────────────────────
  function deviceTitle(device) {
    return device.name || device.app || 'An app';
  }

  // One device: its name, app, when it was last seen, where and what it plays. `options.quality`
  // adds the quality choice; `options.forget` (the route to DELETE it at) adds Remove from list.
  function deviceRow(device, options) {
    options = options || {};
    var playing = device.playing ? el('div', { class: 'fam-row-meta fam-playing' }, F.icon('radio'), el('span', null, 'Playing ', el('strong', null, device.playing.title), device.playing.artist ? ' by ' + device.playing.artist : '')) : null;
    var side = el('div', { class: 'fam-row-side' });
    if (options.quality) {
      var select = el('select', { class: 'set-input fam-select', 'aria-label': 'Quality for ' + deviceTitle(device), onchange: async function () {
        var reply = await api('PUT', '/me/devices/' + encodeURIComponent(device.id), { quality: select.value });
        if (!reply.ok) return F.toast(F.errorOf(reply), true);
        F.toast(select.value === 'Account' ? 'Follows your audio quality' : 'The app decides');
      } },
      el('option', { value: 'Account' }, 'Use my audio quality'),
      el('option', { value: 'App' }, 'Let this app decide'));
      select.value = device.quality || 'Account';
      side.appendChild(select);
    }
    if (options.forget) {
      side.appendChild(el('button', { class: 'btn btn-sm btn-ghost', type: 'button', onclick: async function () {
        if (!confirmIn('Remove ' + deviceTitle(device) + ' from the list? It shows up again the next time it signs in. To stop it signing in, change the password.')) return;
        var reply = await api('DELETE', options.forget + encodeURIComponent(device.id));
        if (!reply.ok) return F.toast(F.errorOf(reply), true);
        F.toast('Removed from the list');
        if (options.after) options.after();
      } }, 'Remove from list'));
    }
    var web = /browser|navidrome/i.test(device.app || '') || /browser/i.test(device.name || '');
    return el('li', { class: 'fam-row stack' },
      el('span', { class: 'fam-row-ico', 'aria-hidden': 'true' }, F.icon(web ? 'globe' : 'phone')),
      el('div', { class: 'fam-row-main' },
        el('div', { class: 'fam-row-title' }, deviceTitle(device)),
        el('div', { class: 'fam-row-meta' },
          [options.owner ? device.username : null, device.name ? device.app : null, F.ago(device.lastSeen)].filter(Boolean).join(' · ')),
        el('div', { class: 'fam-badges' }, placeBadge(device.place), device.quality === 'App' ? badge('App decides') : null),
        playing),
      side);
  }

  // Sign out everywhere: a new password, which every app then needs.
  function signOutEverywhere() {
    location.hash = '#/account?password';
  }

  async function viewDevices(main) {
    var reply = await api('GET', '/me/devices');
    if (!reply.ok) return failed(main, reply);
    var devices = reply.data;
    var list = el('ul', { class: 'fam-list' });
    devices.forEach(function (d) {
      list.appendChild(deviceRow(d, { quality: true, forget: '/me/devices/', after: function () { viewDevices(main); } }));
    });
    var everywhere = session.managed
      ? el('button', { class: 'btn', type: 'button', onclick: signOutEverywhere }, F.icon('signin'), 'Sign out everywhere')
      : null;
    F.clear(main);
    F.append(main, [
      pageHeader('phone', 'Devices', session.managed
        ? 'Every app that signed in with your login: where it was last seen and what it plays. Octo adds them as they sign in.'
        : 'The apps Octo has seen you use. Choose how each picks its audio quality.', everywhere),
      card(null, null, null, devices.length ? list : empty('phone',
        session.managed ? 'No devices yet. Sign in on any app with your login, and it shows up here.' : 'Octo has not seen a device of yours yet. Play something in any app and it shows up here.',
        session.managed ? el('a', { class: 'btn btn-primary', href: '#/account' }, F.icon('key'), 'Your login') : null)),
      session.managed ? el('p', { class: 'fam-help fam-gap' }, 'Remove from list only tidies the list. To sign every device out, change your password: each app then needs the new one.') : null
    ]);
  }

  // ── You: library, saved, requests ────────────────────────────────────────────────────
  function cover(id, words) {
    var tile = el('div', { class: 'fam-cover', 'aria-hidden': 'true' }, F.initials(words));
    if (id) {
      var img = el('img', { class: 'fam-cover', alt: '', loading: 'lazy', src: '/api/family/cover/' + encodeURIComponent(id) + '?size=96' });
      img.addEventListener('error', function () { img.replaceWith(tile); });
      return img;
    }
    return tile;
  }

  async function viewLibrary(main) {
    var offset = 0;
    var size = 100;
    var list = el('ul', { class: 'fam-list' });
    var more = el('button', { class: 'btn', type: 'button', hidden: true }, 'Show more');
    var summary = el('div');
    var body = el('div', { class: 'fam-gap' }, list);
    var load = async function () {
      var reply = await api('GET', '/me/library?offset=' + offset + '&size=' + size);
      if (!reply.ok) return failed(main, reply);
      var page = reply.data;
      F.clear(summary).appendChild(meter(page.usedBytes, page.limitBytes, page.total + (page.total === 1 ? ' song' : ' songs')));
      if (!page.total) {
        F.clear(body).appendChild(empty('folder', 'Nothing here yet. Songs added for you, and copies you asked for, land in your library.',
          el('a', { class: 'btn', href: '#/saved' }, 'Open saved songs')));
      }
      page.songs.forEach(function (song) {
        var row = el('li', { class: 'fam-row stack' },
          cover(song.coverArt || song.id, song.title),
          el('div', { class: 'fam-row-main' },
            el('div', { class: 'fam-row-title' }, song.title),
            el('div', { class: 'fam-row-meta' }, [song.artist, song.album, F.bytes(song.size)].filter(Boolean).join(' · '))),
          el('div', { class: 'fam-row-side' }, el('button', { class: 'btn btn-sm btn-ghost', type: 'button', onclick: async function () {
            if (!confirmIn('Remove ' + song.title + ' from your library? It frees its space, and you can ask for it again later.')) return;
            var r = await api('DELETE', '/me/library/' + encodeURIComponent(song.id));
            if (!r.ok) return F.toast(F.errorOf(r), true);
            row.remove();
            F.toast('Removed from your library');
          } }, 'Remove')));
        list.appendChild(row);
      });
      offset += page.songs.length;
      more.hidden = offset >= page.total;
    };
    more.addEventListener('click', load);
    F.clear(main);
    F.append(main, [
      pageHeader('folder', 'My library', 'Songs added for you, and how much of your space they take. Remove what you no longer play.'),
      card(null, null, null, summary, body, el('div', { class: 'fam-actions' }, more))
    ]);
    await load();
  }

  async function viewSaved(main) {
    var reply = await api('GET', '/me/saved');
    if (!reply.ok) return failed(main, reply);
    var mayAsk = session.abilities && session.abilities.addToLibrary !== 'SaveOnly';
    var songs = el('ul', { class: 'fam-list' });
    var saved = reply.data;
    saved.songs.forEach(function (song) {
      var row = el('li', { class: 'fam-row stack' },
        cover(song.coverArt, song.title),
        el('div', { class: 'fam-row-main' },
          el('div', { class: 'fam-row-title' }, song.title),
          el('div', { class: 'fam-row-meta' }, [song.artist, song.album].filter(Boolean).join(' · '))),
        el('div', { class: 'fam-row-side' },
          mayAsk ? el('button', { class: 'btn btn-sm btn-primary', type: 'button', onclick: function () { askSheet(song); } }, 'Ask for a copy') : null,
          el('button', { class: 'btn btn-sm btn-ghost', type: 'button', onclick: async function () {
            var r = await api('DELETE', '/me/saved/' + encodeURIComponent(song.id));
            if (!r.ok) return F.toast(F.errorOf(r), true);
            row.remove();
            F.toast('Removed');
          } }, 'Remove')));
      songs.appendChild(row);
    });
    saved.albums.forEach(function (album) {
      var row = el('li', { class: 'fam-row stack' },
        cover(album.coverArt, album.name),
        el('div', { class: 'fam-row-main' },
          el('div', { class: 'fam-row-title' }, album.name),
          el('div', { class: 'fam-row-meta' }, ['Album', album.artist, album.songCount ? album.songCount + ' songs' : null].filter(Boolean).join(' · '))),
        el('div', { class: 'fam-row-side' }, el('button', { class: 'btn btn-sm btn-ghost', type: 'button', onclick: async function () {
          var r = await api('DELETE', '/me/saved/' + encodeURIComponent(album.id));
          if (!r.ok) return F.toast(F.errorOf(r), true);
          row.remove();
        } }, 'Remove')));
      songs.appendChild(row);
    });
    F.clear(main);
    F.append(main, [
      pageHeader('heart', 'Saved', 'Songs and albums you hearted from outside the library. They play from where Octo found them' + (mayAsk ? ', and you can ask for a copy of your own.' : '.')),
      card(null, null, null, saved.songs.length || saved.albums.length ? songs
        : empty('heart', 'Nothing saved yet. Heart a song Octo found outside the library, in any app, and it shows up here.'))
    ]);
  }

  // ── You: Import ──────────────────────────────────────────────────────────────────────
  // Only TuneMyMusic's own https pages are opened from the guide.
  function exportUrl(url) {
    try {
      var parsed = new URL(url);
      return parsed.protocol === 'https:' && (parsed.hostname === 'tunemymusic.com' || parsed.hostname === 'www.tunemymusic.com') ? parsed.href : null;
    } catch (e) {
      return null;
    }
  }

  function approvalLine(approval) {
    return 'Your lists are in. ' + ((approval && approval.by) || 'The owner') + ' approves downloads before Octo fetches the missing songs.';
  }

  async function viewImport(main) {
    var reply = await api('GET', '/me/imports');
    if (!reply.ok) return failed(main, reply);
    var data = reply.data;
    var services = (data.services || []).filter(function (s) { return exportUrl(s.exportUrl); });
    var picked = null;

    var grid = el('div', { class: 'imp-svc-grid', role: 'group', 'aria-label': 'Your music service' });
    var steps = el('ol', { class: 'lfm-steps' });
    var open = el('a', { class: 'btn btn-primary fam-big', target: '_blank', rel: 'noopener noreferrer', href: 'https://www.tunemymusic.com/' }, F.icon('out'), el('span', null, 'Open TuneMyMusic'));
    var guide = el('div', { class: 'imp-guide', hidden: true }, steps, open);
    function drawServices() {
      F.clear(grid);
      services.forEach(function (service) {
        var on = picked === service.id;
        grid.appendChild(el('button', {
          type: 'button',
          class: 'imp-svc' + (on ? ' selected' : ''),
          'aria-pressed': on ? 'true' : 'false',
          onclick: function () { picked = on ? null : service.id; drawServices(); }
        }, el('span', { class: 'imp-svc-check', 'aria-hidden': 'true' }, F.icon('check')), el('span', { class: 'imp-svc-name' }, service.name)));
      });
      var service = services.filter(function (s) { return s.id === picked; })[0];
      guide.hidden = !service;
      if (!service) return;
      F.clear(steps);
      [
        'Tap Open TuneMyMusic below.',
        'Tap the ' + service.tile + ' tile and sign in to ' + service.name + '.',
        'Pick your playlists or Liked songs.',
        'Export to file, as CSV.',
        'Come back here and upload the file.'
      ].forEach(function (step) { steps.appendChild(el('li', null, step)); });
      open.href = exportUrl(service.exportUrl);
      open.querySelector('span').textContent = 'Open TuneMyMusic for ' + service.name;
    }
    drawServices();

    var notice = el('p', { class: 'notice', role: 'status', hidden: !(data.approval && data.lists && data.lists.length) }, approvalLine(data.approval));

    async function added(answer) {
      F.toast(answer.message || 'Imported');
      if (data.approval) {
        notice.hidden = false;
        notice.textContent = approvalLine(data.approval);
      }
      var again = await api('GET', '/me/imports');
      if (again.ok) drawLists(again.data.lists || []);
    }

    var file = el('input', { class: 'set-input', type: 'file', id: 'fam-imp-file', accept: '.csv,.txt,.json,.zip,text/csv,text/plain,application/json,application/zip' });
    var upload = el('button', { class: 'btn btn-primary btn-block fam-big', type: 'button', onclick: async function () {
      var chosen = file.files && file.files[0];
      if (!chosen) return F.toast('Choose a file first.', true);
      var form = new FormData();
      form.append('file', chosen, chosen.name);
      upload.disabled = true;
      upload.textContent = 'Reading the file';
      var res = null;
      var answer = {};
      try {
        res = await fetch('/api/family/me/imports/file', { method: 'POST', credentials: 'same-origin', headers: { 'X-Octo-Family': '1' }, body: form });
        answer = await res.json().catch(function () { return {}; });
      } catch (e) {
        answer = { error: 'Octo did not answer. Check your connection and try again.' };
      }
      upload.disabled = false;
      upload.textContent = 'Upload the file';
      if (!res || !res.ok) return F.toast(answer.message || answer.error || 'Octo could not read that file.', true);
      file.value = '';
      added(answer);
    } }, 'Upload the file');

    var name = el('input', { class: 'set-input', type: 'text', id: 'fam-imp-name', autocomplete: 'off', placeholder: 'Pasted list' });
    var text = el('textarea', { class: 'set-input imp-paste-box', id: 'fam-imp-text', rows: '6', spellcheck: 'false', placeholder: 'Massive Attack - Teardrop' });
    var paste = el('button', { class: 'btn btn-block fam-big', type: 'button', onclick: async function () {
      if (!text.value.trim()) return F.toast('Paste a list first.', true);
      paste.disabled = true;
      var r = await api('POST', '/me/imports/text', { name: name.value.trim(), text: text.value });
      paste.disabled = false;
      if (!r.ok) return F.toast((r.data && r.data.message) || F.errorOf(r), true);
      text.value = '';
      name.value = '';
      added(r.data);
    } }, 'Import the list');

    var lists = el('ul', { class: 'fam-list' });
    var listsCard = card('Your lists', 'What Octo has of each list you brought in.', null, lists);
    function drawLists(rows) {
      F.clear(lists);
      if (!rows.length) {
        lists.appendChild(el('li', { class: 'fam-quiet' }, 'Nothing imported yet.'));
        return;
      }
      rows.forEach(function (list) {
        lists.appendChild(el('li', { class: 'fam-row' },
          el('span', { class: 'fam-row-ico', 'aria-hidden': 'true' }, F.icon('playlist')),
          el('div', { class: 'fam-row-main' },
            el('div', { class: 'fam-row-title' }, list.name),
            el('div', { class: 'fam-row-meta' }, list.have + ' in the library · ' + list.missing + ' missing'))));
      });
    }
    drawLists(data.lists || []);

    F.clear(main);
    F.append(main, [
      pageHeader('playlist', 'Import', 'Bring your playlists and liked songs from any music service.'),
      data.libraryProblem ? el('p', { class: 'notice notice-warn' }, data.libraryProblem) : null,
      notice,
      card('Get my music', 'TuneMyMusic exports your music from any service to a file for free. Pick your service for the steps.', null, grid, guide),
      card('Upload the file', 'The CSV or TXT file TuneMyMusic made. On a phone it is in your Downloads.', null,
        el('div', { class: 'fam-form' }, el('label', { class: 'fam-label', for: 'fam-imp-file' }, 'File', file), upload)),
      card('Paste a list', 'One song a line, written Artist - Title.', null,
        el('div', { class: 'fam-form' },
          el('label', { class: 'fam-label', for: 'fam-imp-name' }, 'Name', name),
          el('label', { class: 'fam-label', for: 'fam-imp-text' }, 'Songs', text),
          paste)),
      listsCard
    ]);
  }

  function askSheet(song) {
    var max = (session.abilities && session.abilities.requestQuality) || 'Best';
    var rank = { Mp3: 0, Flac: 1, Best: 2 };
    var options = [['Best', 'Best there is'], ['Flac', 'FLAC'], ['Mp3', 'MP3']].filter(function (o) { return rank[o[0]] <= rank[max]; });
    var quality = options[0][0];
    var send = el('button', { class: 'btn btn-primary', type: 'button', onclick: async function () {
      var reply = await api('POST', '/me/requests', { id: song.id, quality: quality });
      if (!reply.ok) return F.toast(F.errorOf(reply), true);
      F.closeSheet();
      F.toast(reply.data.state === 'Done' ? 'It is in your library' : reply.data.state === 'Approved' ? 'Approved' : 'Asked. You will hear back.');
    } }, 'Ask for a copy');
    F.openSheet('Ask for a copy', el('div', { class: 'fam-form' },
      el('div', { class: 'fam-label' }, 'Quality', segmented(options, quality, function (v) { quality = v; }, 'Quality'))),
    { sub: song.title + (song.artist ? ' by ' + song.artist : ''), foot: [el('span'), send] });
  }

  function requestRow(request, options) {
    options = options || {};
    var side = el('div', { class: 'fam-row-side' });
    var note = el('input', { class: 'set-input', placeholder: 'A note for them (optional)', 'aria-label': 'Note for ' + (request.displayName || request.username || 'them') });
    if (options.decide && request.state === 'Pending') {
      side.appendChild(el('button', { class: 'btn btn-sm btn-primary', type: 'button', onclick: function () { options.decide(request, true, note.value); } }, 'Approve'));
      side.appendChild(el('button', { class: 'btn btn-sm btn-ghost', type: 'button', onclick: function () { options.decide(request, false, note.value); } }, 'Decline'));
    }
    if (options.cancel && request.state === 'Pending') {
      side.appendChild(el('button', { class: 'btn btn-sm btn-ghost', type: 'button', onclick: function () { options.cancel(request); } }, 'Cancel'));
    }
    if (options.retry && request.state === 'Failed' && request.quality === 'Flac') {
      side.appendChild(el('button', { class: 'btn btn-sm', type: 'button', onclick: function () { options.retry(request); } }, 'Try MP3'));
    }
    var meta = [request.artist, request.album].filter(Boolean).join(' · ');
    var who = options.decide ? request.displayName + ' asked ' + F.ago(request.created).toLowerCase() : 'Asked ' + F.ago(request.created).toLowerCase();
    var warning = options.fullFor && options.fullFor(request) ? el('div', { class: 'fam-row-meta fam-warn-text' }, options.fullFor(request)) : null;
    return el('li', { class: 'fam-row stack' },
      cover(request.coverArt, request.title),
      el('div', { class: 'fam-row-main' },
        el('div', { class: 'fam-row-title' }, request.title || request.target),
        meta ? el('div', { class: 'fam-row-meta' }, meta) : null,
        el('div', { class: 'fam-row-meta' }, who),
        el('div', { class: 'fam-badges' }, stateBadge(request.state), badge(qualityWord(request.quality))),
        request.note ? el('div', { class: 'fam-row-meta' }, '“' + request.note + '”') : null,
        request.failure ? el('div', { class: 'fam-row-meta fam-bad-text' }, request.failure) : null,
        warning,
        options.decide && request.state === 'Pending' ? el('div', { class: 'fam-note-field' }, note) : null),
      side);
  }

  async function viewRequests(main) {
    var reply = await api('GET', '/me/requests');
    if (!reply.ok) return failed(main, reply);
    var list = el('ul', { class: 'fam-list' });
    reply.data.forEach(function (request) {
      list.appendChild(requestRow(request, { cancel: async function (r) {
        if (!confirmIn('Cancel your request for ' + (r.title || 'this song') + '?')) return;
        var answer = await api('DELETE', '/me/requests/' + encodeURIComponent(r.id));
        if (!answer.ok) return F.toast(F.errorOf(answer), true);
        F.toast('Canceled');
        viewRequests(main);
      }, retry: async function (r) {
        var answer = await api('POST', '/me/requests', { id: r.target, quality: 'Mp3' });
        if (!answer.ok) return F.toast(F.errorOf(answer), true);
        F.toast(answer.data.state === 'Pending' ? 'Asked for an MP3. You will hear back.' : 'Asked for an MP3');
        viewRequests(main);
      } }));
    });
    F.clear(main);
    F.append(main, [
      pageHeader('tray', 'Requests', 'Copies you asked for, and what became of them. Cancel one that is still waiting.'),
      card(null, null, null, reply.data.length ? list
        : empty('tray', 'No requests yet. Ask for a copy of a saved song, here or in the Octo app.', el('a', { class: 'btn', href: '#/saved' }, 'Open saved songs')))
    ]);
  }

  // ── You: Ready to play ───────────────────────────────────────────────────────────────
  var KEEP_READY_KINDS = { import: 'Imported', mix: 'Mix', chart: 'Chart', family: 'Family playlist' };

  function keepReadyMeta(list) {
    var parts = [KEEP_READY_KINDS[list.kind] || 'Playlist'];
    if (!list.outside) {
      parts.push('every song is in your library');
    } else {
      parts.push(list.previewsReady + ' of ' + list.outside + ' ready');
      if (list.flacReady) parts.push(list.flacReady + ' as FLAC');
    }
    return parts.join(' · ');
  }

  async function viewReady(main) {
    var reply = await api('GET', '/me/keep-ready');
    if (!reply.ok) return failed(main, reply);
    var view = reply.data;
    var picked = function () {
      return view.lists.filter(function (list) { return list.picked; }).map(function (list) { return list.id; });
    };
    // Saves go one at a time, each from the choices as the clicks so far left them.
    var saving = Promise.resolve();
    var save = function () {
      saving = saving.then(async function () {
        var r = await api('PUT', '/me/keep-ready', { lists: picked(), mostPlayed: view.mostPlayed });
        if (!r.ok) F.toast(F.errorOf(r), true);
        else view = r.data;
        draw();
      });
    };
    var draw = function () {
      var most = switchControl(view.mostPlayed, 'Also the playlists I play most', function () {
        view.mostPlayed = most.input.checked;
        save();
      });
      var rows = view.lists.map(function (list) {
        return el('button', {
          type: 'button',
          class: 'kr-list',
          'aria-pressed': list.picked ? 'true' : 'false',
          'data-id': list.id,
          onclick: function () {
            list.picked = !list.picked;
            this.setAttribute('aria-pressed', list.picked ? 'true' : 'false');
            save();
          }
        },
        el('span', { class: 'kr-check', 'aria-hidden': 'true' }, F.icon('check')),
        el('span', { class: 'kr-text' },
          el('span', { class: 'kr-name' }, list.name),
          el('span', { class: 'kr-meta' }, keepReadyMeta(list))),
        list.byMostPlayed ? el('span', { class: 'kr-tag' }, 'Played most') : null);
      });
      var note = '';
      if (!view.enabled) note = 'Your family’s server keeps nothing ready right now. Ask the person who runs it to turn on Download songs ahead.';
      else if (!view.lists.length) note = 'None of your playlists can hold songs that aren’t in your library yet. Imported playlists streamed without downloading, mixes and charts show up here.';
      else if (view.mostPlayed && !view.listeningOn) note = 'Finding the playlists you play most needs Listening on.';
      // The row that had the focus keeps it through the redraw.
      var focused = document.activeElement && document.activeElement.closest && document.activeElement.closest('.kr-list');
      var focusedId = focused ? focused.getAttribute('data-id') : null;
      F.clear(main);
      F.append(main, [
        pageHeader('stack', 'Ready to play', 'Playlists you pick start right away in any app, even songs that aren’t in your library yet.'),
        rowsCard([
          setRow('Also the playlists I play most', 'Keeps the three playlists you play from most ready too, from what you play in any app.', most.node),
          rows.length ? el('div', { class: 'kr-lists', role: 'group', 'aria-label': 'Playlists to keep ready' }, rows) : null,
          note ? el('div', { class: 'set-row kr-note-row' }, el('span', { class: 'set-info-d kr-note', role: 'status' }, note)) : null
        ])
      ]);
      if (focusedId) {
        var again = main.querySelector('.kr-list[data-id="' + CSS.escape(focusedId) + '"]');
        if (again) again.focus();
      }
    };
    draw();
  }

  // ── You: Audio quality and Account ───────────────────────────────────────────────────
  async function viewQuality(main) {
    var reply = await api('GET', '/me');
    if (!reply.ok) return failed(main, reply);
    var q = reply.data.quality;
    var help = el('p', { class: 'fam-help' });
    var save = async function (place, value) {
      var body = {};
      body[place] = value;
      var r = await api('PUT', '/me/quality', body);
      if (!r.ok) return F.toast(F.errorOf(r), true);
      q = r.data;
      F.toast(place === 'home' ? 'At home: ' + value : 'Away: ' + value);
      describe();
    };
    var describe = function () {
      var lines = ['At home: ' + QUALITY_HELP[q.home] + ' Away: ' + QUALITY_HELP[q.away]];
      if (q.familyLimitKbps > 0) lines.push('Your family plan limits listening to ' + q.familyLimitKbps + ' kbps.');
      if (q.familyAwayLimitKbps > 0) lines.push('Your family plan limits away listening to ' + q.familyAwayLimitKbps + ' kbps.');
      help.textContent = lines.join(' ');
    };
    describe();
    F.clear(main);
    F.append(main, [
      pageHeader('sliders', 'Audio quality', 'What Octo sends your devices, at home and away. Each device can follow this or let its app decide, under Devices.'),
      rowsCard([
        setRow('At home', 'On your home network.', segmented(QUALITIES, q.home, function (v) { save('home', v); }, 'Quality at home')),
        setRow('Away from home', 'Everywhere else, often on mobile data.', segmented(QUALITIES, q.away, function (v) { save('away', v); }, 'Quality away from home')),
        el('div', { class: 'set-row' }, help)
      ]),
      el('p', { class: 'fam-help fam-gap' }, 'Songs from outside the library already stream at about 128 to 160 kbps, so these choices leave them as they are.')
    ]);
  }

  async function viewAccount(main) {
    var replies = await Promise.all([api('GET', '/me'), api('GET', '/me/login')]);
    if (!replies[0].ok) return failed(main, replies[0]);
    var password = passwordCard();
    F.clear(main);
    F.append(main, [
      pageHeader('key', 'Account', 'Your login for every app, your password, your Last.fm scrobbles and where your notifications go.'),
      replies[1].ok ? loginCard(replies[1].data, function () { focusPassword(password); }) : null,
      password,
      handOverCard(),
      lastFmCard(),
      ntfyCard(replies[0].data)
    ]);
    if (/[?&]password/.test(location.hash)) focusPassword(password);
  }

  function focusPassword(card) {
    var calm = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    card.scrollIntoView({ behavior: calm ? 'auto' : 'smooth', block: 'start' });
    var input = card.querySelector('input');
    if (input) input.focus({ preventScroll: true });
  }

  // "Your login": what to type in any app. `onChange` runs from its Change password button.
  function loginCard(login, onChange) {
    return card('Your login', 'One login for every app: Octo, Symfonium, Feishin and any Subsonic app.', null,
      F.loginCard(login, { changePassword: onChange || function () { location.hash = '#/account?password'; } }));
  }

  // Signing in on a new device by QR code starts from an app that is already signed in.
  function handOverCard() {
    return card('Sign in on another device', null, null,
      el('p', { class: 'fam-where-uses' }, F.icon('qr'),
        el('span', null, 'Open the Octo app on a device that is already signed in and choose ', el('strong', null, 'Sign in on another device'), '. It shows a QR code: scan it on the new device and allow it, and the new device is signed in without typing.')),
      el('p', { class: 'fam-help' }, 'This page cannot show that code: it never holds your password.'));
  }

  function randomTopic() {
    var letters = 'abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789';
    var bytes = new Uint8Array(24);
    crypto.getRandomValues(bytes);
    return Array.prototype.map.call(bytes, function (b) { return letters.charAt(b % letters.length); }).join('');
  }

  function ntfyCard(me) {
    var input = el('input', { class: 'set-input fam-mono', value: me.ntfyTopic || '', placeholder: 'A private topic name', autocomplete: 'off', spellcheck: 'false', 'aria-label': 'ntfy topic' });
    var save = async function () {
      var reply = await api('PUT', '/me/ntfy', { topic: input.value.trim() });
      if (!reply.ok) return F.toast(F.errorOf(reply), true);
      F.toast(input.value.trim() ? 'Pushes go to this topic now' : 'Pushes are off');
    };
    return card('Notifications', 'A push when a request is approved, declined, added or fails. Install the ntfy app and subscribe to your topic.', null,
      el('div', { class: 'fam-form' },
        el('div', { class: 'set-inline' }, input),
        el('div', { class: 'fam-actions' },
          el('button', { class: 'btn btn-ghost', type: 'button', onclick: function () { input.value = randomTopic(); } }, 'Suggest a private topic'),
          el('button', { class: 'btn btn-primary', type: 'button', onclick: save }, 'Save')),
        el('p', { class: 'fam-help' }, 'Anyone who knows a topic name can read it, so use a long random one. Leave it empty for no pushes.')));
  }

  function lastFmCard() {
    var body = el('div', { class: 'fam-form' }, el('p', { class: 'fam-quiet' }, 'Loading'));
    var node = card('Last.fm', 'Send what you play to your own Last.fm account.', null, body);
    var load = async function () {
      var reply = await api('GET', '/me/lastfm');
      F.clear(body);
      if (!reply.ok) return body.appendChild(el('p', { class: 'field-error' }, F.errorOf(reply)));
      var info = reply.data;
      if (!info.available) {
        return body.appendChild(el('p', { class: 'fam-quiet' }, 'Last.fm is not set up on this server yet. Ask whoever runs it to add a Last.fm API key.'));
      }
      var user = info.user;
      if (user && user.connected) {
        body.appendChild(el('p', null, 'Connected as ', el('strong', null, user.lastFmUser || 'your account'), '.'));
        body.appendChild(el('div', null, el('button', { class: 'btn btn-ghost', type: 'button', onclick: async function () {
          if (!confirmIn('Disconnect Last.fm? What you play stops being sent.')) return;
          var r = await api('POST', '/me/lastfm/disconnect', {});
          if (!r.ok) return F.toast(F.errorOf(r), true);
          F.toast('Disconnected');
          load();
        } }, 'Disconnect')));
        return;
      }
      body.appendChild(el('ol', { class: 'fam-steps' },
        el('li', null, 'Press Connect and approve Octo on last.fm, signed in as you.'),
        el('li', null, 'Come back and press Finish.')));
      body.appendChild(el('div', { class: 'fam-actions' },
        el('button', { class: 'btn btn-primary', type: 'button', onclick: async function () {
          var r = await api('POST', '/me/lastfm/connect', {});
          if (!r.ok) return F.toast(F.errorOf(r), true);
          window.open(r.data.url, '_blank', 'noopener');
        } }, 'Connect'),
        el('button', { class: 'btn btn-ghost', type: 'button', onclick: async function () {
          var r = await api('POST', '/me/lastfm/finish', {});
          if (!r.ok) return F.toast(r.status === 409 ? 'Last.fm has not seen your approval yet.' : F.errorOf(r), true);
          F.toast('Connected');
          load();
        } }, 'Finish')));
    };
    load();
    return node;
  }

  function passwordCard() {
    var current = el('input', { class: 'set-input', type: 'password', autocomplete: 'current-password', id: 'pw-current' });
    var next = el('input', { class: 'set-input', type: 'password', autocomplete: 'new-password', id: 'pw-next', minlength: 8 });
    var error = el('p', { class: 'field-error', role: 'alert' });
    var form = el('form', { class: 'fam-form', onsubmit: async function (e) {
      e.preventDefault();
      error.textContent = '';
      if (!confirmIn('Change your password? Every app and browser signed in with the old one is signed out, and needs the new one.')) return;
      var reply = await api('PUT', '/me/password', { current: current.value, next: next.value });
      if (!reply.ok) { error.textContent = F.errorOf(reply); return; }
      current.value = '';
      next.value = '';
      F.toast('Password changed. Sign in again on your apps with the new one.');
    } },
    el('div', { class: 'set-grid2' },
      el('label', { class: 'fam-label', for: 'pw-current' }, 'Current password', current),
      el('label', { class: 'fam-label', for: 'pw-next' }, 'New password', next, F.strengthHint(next))),
    error,
    el('div', null, el('button', { class: 'btn btn-primary', type: 'submit' }, 'Change password')));
    return card('Password', 'Changing it signs you out everywhere: every app and browser needs the new one.', null, form);
  }

  // ── The outside address ──────────────────────────────────────────────────────────────
  // Where the dashboard keeps it: its Status page, at the public address field.
  var STATUS_ADDRESS = '/admin/#status/public-address';

  // The owner's inline field for the outside address, with Save and the reachability check
  // in plain words. `onSaved(publicUrl, words)` runs after a save.
  function addressField(onSaved) {
    var input = el('input', { class: 'set-input fam-mono', type: 'text', inputmode: 'url', autocomplete: 'off', spellcheck: 'false', autocapitalize: 'none',
      placeholder: 'https://music.example.com', 'aria-label': 'Outside address', value: session.publicUrl || session.suggestedPublicUrl || '' });
    var result = el('p', { class: 'inline-note', role: 'status', hidden: true });
    var save = el('button', { class: 'btn btn-primary', type: 'submit' }, 'Save');
    var form = el('form', { class: 'fam-address', onsubmit: async function (e) {
      e.preventDefault();
      save.disabled = true;
      result.hidden = false;
      result.className = 'inline-note busy';
      result.textContent = 'Saving';
      var r = await api('PUT', '/public-address', { url: input.value.trim() });
      if (!r.ok) {
        save.disabled = false;
        result.className = 'inline-note error';
        result.textContent = F.errorOf(r);
        return;
      }
      session.publicUrl = r.data.publicUrl;
      var words = 'Saved.';
      if (r.data.publicUrl) {
        result.textContent = 'Saved. Checking it from here';
        var check = await api('POST', '/public-address/check', {});
        words = check.ok ? 'Saved. ' + check.data.message : 'Saved.';
        result.className = 'inline-note ' + (check.ok && check.data.reachable ? 'ok' : 'info');
      } else {
        result.className = 'inline-note ok';
      }
      result.textContent = words;
      save.disabled = false;
      if (onSaved) onSaved(r.data.publicUrl, words);
    } },
    el('div', { class: 'set-inline' }, input, save),
    session.suggestedPublicUrl && !session.publicUrl ? el('p', { class: 'fam-help' }, 'Octo saw your reverse proxy use ', el('code', null, session.suggestedPublicUrl), '. Check it is right, then save.') : null,
    result);
    return form;
  }

  // The notice when links only work at home: what it means, the dashboard's control, and the
  // inline field for the owner.
  function addressNotice(onSaved) {
    return el('section', { class: 'notice notice-warn fam-address-notice' },
      el('p', null, el('strong', null, 'Set your outside address.'), ' Without it, invite links and everyone’s apps only work on your home network.'),
      session.owner
        ? el('div', { class: 'fam-stack' },
          el('div', null, el('a', { class: 'btn btn-sm', href: STATUS_ADDRESS }, F.icon('out'), 'Set it on the Status page')),
          el('p', { class: 'fam-help' }, 'Or type it here:'),
          addressField(onSaved))
        : el('p', { class: 'fam-help' }, 'Ask whoever runs this server to set its outside address.'));
  }

  // ── Family: Members ──────────────────────────────────────────────────────────────────
  async function viewMembers(main) {
    if (!session.familyEnabled) return viewFamilyOff(main);
    var replies = await Promise.all([api('GET', '/members'), api('GET', '/unrated'), api('GET', '/devices')]);
    if (!replies[0].ok) return failed(main, replies[0]);
    var members = replies[0].data;
    var devices = replies[2].ok ? replies[2].data : [];
    var addButton = function (words) {
      return el('button', { class: 'btn btn-primary', type: 'button', onclick: addMemberSheet }, F.icon('plus'), words);
    };
    var parts = [pageHeader('users', 'Members', 'Everyone you share your music with. Add someone, send them their link, and choose what they can do.', addButton('Add member'))];
    if (!session.publicUrl) parts.push(addressNotice(function () { route(); }));
    if (session.links === 'Copy' && session.linksMessage) {
      parts.push(el('div', { class: 'notice notice-warn', role: 'status' }, session.linksMessage));
    }
    if (session.proxyWarning) {
      parts.push(el('div', { class: 'notice notice-warn', role: 'alert' }, session.proxyWarning));
    }
    var unrated = replies[1].ok ? replies[1].data : null;
    if (unrated && unrated.count > 0) {
      var mark = el('button', { class: 'btn btn-sm', type: 'button', onclick: async function () {
        mark.disabled = true;
        var r = await api('POST', '/unrated/mark', {});
        mark.disabled = false;
        F.toast(r.ok ? r.data.message : F.errorOf(r), !r.ok);
      } }, 'Mark them now');
      parts.push(el('div', { class: 'notice fam-notice-row' },
        el('div', null,
          el('strong', null, unrated.count + (unrated.count === 1 ? ' song has' : ' songs have') + ' no explicit mark. '),
          'Kid profiles cannot tell whether those are clean. Octo can look them up and mark them.'),
        mark));
    }
    if (!members.length) {
      parts.push(card(null, null, null, empty('users', 'No one yet. Add the first member of your family: they get a link to choose a password, then sign in on any app.',
        addButton('Add the first member'))));
    } else {
      parts.push(el('div', { class: 'fam-grid two' }, members.map(function (m) {
        return memberCard(m, devices.filter(function (d) { return d.username.toLowerCase() === m.username.toLowerCase(); }));
      })));
    }
    F.clear(main);
    F.append(main, parts);
  }

  async function viewFamilyOff(main) {
    var parts = [pageHeader('users', 'Family is off', 'Family gives the people you share your music with their own sign-in, library and devices.')];
    if (session.role === 'Owner') {
      var report = el('div');
      var button = el('button', { class: 'btn btn-primary', type: 'button', onclick: async function () {
        button.disabled = true;
        var r = await api('POST', '/enable', { enabled: true });
        button.disabled = false;
        if (!r.ok) { F.clear(report).appendChild(el('p', { class: 'notice notice-error' }, F.errorOf(r))); return; }
        F.toast('Family is on');
        start();
      } }, 'Turn on Family');
      parts.push(card('Turn it on', null, null,
        el('p', null, 'Octo makes a folder for each member at ', el('code', null, session.membersPath), '. Navidrome needs to see that folder at the same path.'),
        el('div', { class: 'fam-actions' }, button),
        report));
    } else {
      parts.push(card(null, null, null, el('p', null, 'Ask whoever runs this server to turn Family on.')));
    }
    F.clear(main);
    F.append(main, parts);
  }

  function memberCard(m, devices) {
    var limit = m.storageLimitGb > 0 ? m.storageLimitGb * 1e9 : 0;
    var badges = [badge(ROLE_WORDS[m.role] || m.role, 'accent')];
    if (m.suspended) badges.push(badge('Paused', 'bad'));
    if (!m.joined) badges.push(badge('Has not joined yet', 'warn'));
    if (m.needsPassword) badges.push(badge('Needs a new password', 'warn'));
    if (m.playingNow) badges.push(badge('Playing now', 'good'));
    if (m.pendingRequests) badges.push(badge(m.pendingRequests + (m.pendingRequests === 1 ? ' request waiting' : ' requests waiting'), 'warn'));
    var more = el('details', { class: 'fam-more' },
      el('summary', { class: 'btn btn-sm btn-ghost', 'aria-label': 'More for ' + m.displayName }, 'More', F.icon('caretDown')),
      el('div', { class: 'fam-more-menu' },
        el('button', { type: 'button', onclick: function () { suspend(m); } }, m.suspended ? 'Resume listening' : 'Pause listening'),
        el('button', { type: 'button', onclick: function () { repair(m); } }, 'Repair their account'),
        el('button', { type: 'button', class: 'fam-danger', onclick: function () { removeSheet(m); } }, 'Remove from the family')));
    return el('section', { class: 'info-card fam-card fam-member' },
      el('div', { class: 'fam-member-head' },
        F.avatar(m.displayName, 'fam-avatar-lg'),
        el('div', { class: 'fam-row-main' },
          el('h2', { class: 'fam-member-name' }, m.displayName),
          el('div', { class: 'fam-row-meta' }, m.username + ' · ' + m.devices + (m.devices === 1 ? ' device' : ' devices')),
          el('div', { class: 'fam-badges' }, badges))),
      meter(m.storageUsedBytes, limit, 'Their library'),
      el('div', { class: 'fam-actions' },
        el('button', { class: 'btn btn-sm btn-primary', type: 'button', onclick: function () { editMemberSheet(m); } }, 'Edit'),
        el('button', { class: 'btn btn-sm', type: 'button', onclick: function () { memberDevicesSheet(m, devices); } }, 'Devices'),
        m.joined
          ? el('button', { class: 'btn btn-sm', type: 'button', onclick: function () { resetPassword(m); } }, 'Reset password')
          : el('button', { class: 'btn btn-sm', type: 'button', onclick: function () { inviteDialog(m, null); } }, 'Send their link'),
        more));
  }

  function refreshFamily() {
    if (currentView() === 'members') route();
  }

  async function suspend(m) {
    if (!m.suspended && !confirmIn('Pause ' + m.displayName + '? Their devices stop playing until you resume them.')) return;
    var r = await api('PUT', '/members/' + encodeURIComponent(m.username), { suspended: !m.suspended });
    if (!r.ok) return F.toast(F.errorOf(r), true);
    F.toast(r.data.suspended ? m.displayName + ' is paused' : m.displayName + ' can listen again');
    refreshFamily();
  }

  async function repair(m) {
    var r = await api('POST', '/members/' + encodeURIComponent(m.username) + '/repair', {});
    F.toast(r.ok ? 'Repaired ' + m.displayName + '’s account' : F.errorOf(r), !r.ok);
  }

  function memberDevicesSheet(m, devices) {
    var list = el('ul', { class: 'fam-list' });
    devices.forEach(function (d) {
      list.appendChild(deviceRow(d, { forget: '/devices/', after: function () { F.closeSheet(); refreshFamily(); } }));
    });
    var reset = m.joined ? el('button', { class: 'btn', type: 'button', onclick: function () { resetPassword(m); } }, F.icon('key'), 'Reset password') : null;
    F.openSheet(m.displayName + '’s devices', devices.length ? list : empty('phone', m.joined ? 'No devices yet. They show up as ' + m.displayName + ' signs in on apps.' : m.displayName + ' has not joined yet. Their devices show up once they sign in.'),
      { sub: 'Octo adds devices as they sign in. To sign ' + m.displayName + ' out everywhere, reset their password.', foot: [el('span'), reset] });
  }

  async function librariesPicker(current, onChange) {
    var reply = await api('GET', '/libraries');
    var shared = reply.ok ? reply.data.filter(function (l) { return !l.personal; }) : [];
    var all = current === 'All' || current === undefined;
    var chosen = Array.isArray(current) ? current.slice() : [];
    var box = el('div', { class: 'set-card fam-rows' });
    var allSwitch = switchControl(all, 'Every shared library');
    var ids = el('div', { hidden: all });
    shared.forEach(function (l) {
      var check = switchControl(chosen.indexOf(l.id) >= 0 || all, l.name);
      check.input.setAttribute('data-id', l.id);
      check.input.addEventListener('change', function () { if (onChange) onChange(); });
      ids.appendChild(setRow(l.name, l.path, check.node));
    });
    allSwitch.input.addEventListener('change', function () { ids.hidden = allSwitch.input.checked; if (onChange) onChange(); });
    box.appendChild(setRow('Every shared library', 'Libraries you add later are shared too.', allSwitch.node));
    box.appendChild(ids);
    box.value = function () {
      if (allSwitch.input.checked) return 'All';
      return Array.prototype.filter.call(ids.querySelectorAll('input'), function (i) { return i.checked; })
        .map(function (i) { return Number(i.getAttribute('data-id')); });
    };
    return box;
  }

  async function addMemberSheet() {
    var preset = 'Listener';
    var username = el('input', { class: 'set-input', id: 'new-username', autocapitalize: 'none', spellcheck: 'false', autocomplete: 'off', placeholder: 'alex', required: true });
    var displayName = el('input', { class: 'set-input', id: 'new-name', autocomplete: 'off', placeholder: 'Alex' });
    var presetHelp = el('p', { class: 'fam-help' }, PRESETS[preset]);
    var presets = [['Listener', 'Listener'], ['Member', 'Member'], ['Kid', 'Kid']];
    if (session.role === 'Owner') presets.push(['CoAdmin', 'Co-admin']);
    var away = switchControl(true, 'Allow listening away from home');
    var error = el('p', { class: 'field-error', role: 'alert' });
    var libraries = await librariesPicker('All');
    var adopt = false;
    var submit = el('button', { class: 'btn btn-primary', type: 'submit', form: 'add-member' }, 'Add');
    var form = el('form', { class: 'fam-form', id: 'add-member', onsubmit: async function (e) {
      e.preventDefault();
      error.textContent = '';
      submit.disabled = true;
      var r = await api('POST', '/members', {
        username: username.value.trim().toLowerCase(),
        displayName: displayName.value.trim(),
        preset: preset,
        sharedLibraries: libraries.value(),
        adopt: adopt,
        away: away.input.checked
      });
      submit.disabled = false;
      if (!r.ok) {
        error.textContent = F.errorOf(r);
        if (r.status === 409 && /already has a Navidrome account/.test(F.errorOf(r))) {
          adopt = true;
          submit.textContent = 'Bring them into the family';
        }
        return;
      }
      inviteDialog(r.data.member, r.data);
      refreshFamily();
    } },
    el('div', { class: 'set-grid2' },
      el('label', { class: 'fam-label', for: 'new-name' }, 'Name', displayName, el('span', { class: 'fam-help' }, 'How the family sees them.')),
      el('label', { class: 'fam-label', for: 'new-username' }, 'Username', username, el('span', { class: 'fam-help' }, 'Lowercase letters, numbers, dots, dashes or underscores.'))),
    el('div', { class: 'fam-label' }, 'What they can do', segmented(presets, preset, function (v) { preset = v; presetHelp.textContent = PRESETS[v]; }, 'Plan'), presetHelp),
    rowsCard([setRow('Allow listening away from home', 'On by default. Off keeps their links and apps to your home network.', away.node)]),
    el('div', { class: 'fam-label' }, 'Shared music they see', libraries),
    error);
    F.openSheet('Add member', form, { side: true, sub: 'They get a link to choose a password, then sign in on any app.', foot: [el('button', { class: 'btn btn-ghost', type: 'button', onclick: F.closeSheet }, 'Cancel'), submit] });
    displayName.focus();
  }

  async function editMemberSheet(m) {
    var preset = m.preset;
    var values = Object.assign({}, m.abilities);
    var display = el('input', { class: 'set-input', id: 'edit-name', value: m.displayName });
    var fields = el('div', { class: 'fam-stack' });
    var libraries = await librariesPicker(m.sharedLibraries, function () { syncSharedWarning(); });
    var presetHelp = el('p', { class: 'fam-help' }, PRESETS[preset]);
    // The storage limit measures the member's own folder, which shared music never reaches, so
    // it is disabled, with a note, while their music goes to the shared library.
    var storage = null;
    var sharedWarning = el('p', { class: 'notice notice-warn', hidden: true },
      'They don’t see every shared library, so songs they add may not show up for them.');
    var syncSharedWarning = function () {
      sharedWarning.hidden = !(values.musicGoesTo === 'Shared' && libraries.value() !== 'All');
    };
    var syncStorage = function () {
      // The warning is not tied to the storage row, which some managers may not see.
      syncSharedWarning();
      if (!storage) return;
      var shared = values.musicGoesTo === 'Shared';
      storage.input.disabled = shared;
      storage.row.querySelector('.set-info-d').textContent = shared ? 'Not used: their music goes to the shared library.' : storage.help;
    };
    var drawFields = function () {
      F.clear(fields);
      storage = null;
      ABILITY_GROUPS.forEach(function (group) {
        if (group.owner && session.role !== 'Owner') return;
        var rows = group.fields.map(function (f) {
          var control;
          if (f.kind === 'switch') {
            var sw = switchControl(values[f.key], f.title, function () { values[f.key] = sw.input.checked; });
            control = sw.node;
          } else if (f.kind === 'number') {
            control = el('input', { class: 'set-input fam-number', type: 'number', min: 0, step: 1, value: values[f.key], inputmode: 'numeric', 'aria-label': f.title, oninput: function () { values[f.key] = Math.max(0, parseInt(control.value, 10) || 0); } });
          } else {
            control = el('select', { class: 'set-input fam-select', 'aria-label': f.title, onchange: function () {
              values[f.key] = control.value;
              if (f.key === 'musicGoesTo') syncStorage();
            } },
              f.options.map(function (o) { return el('option', { value: o[0] }, o[1]); }));
            control.value = values[f.key];
          }
          var row = setRow(f.title, f.help, control);
          if (f.key === 'storageLimitGb') storage = { input: control, row: row, help: f.help };
          return row;
        });
        fields.appendChild(el('div', { class: 'fam-group' }, el('h3', { class: 'fam-group-title' }, group.title), rowsCard(rows)));
      });
      syncStorage();
    };
    drawFields();
    var presets = [['Listener', 'Listener'], ['Member', 'Member'], ['Kid', 'Kid']];
    if (session.role === 'Owner' || m.preset === 'CoAdmin') presets.push(['CoAdmin', 'Co-admin']);
    var error = el('p', { class: 'field-error', role: 'alert' });
    var save = el('button', { class: 'btn btn-primary', type: 'submit', form: 'edit-member' }, 'Save');
    var form = el('form', { class: 'fam-form', id: 'edit-member', onsubmit: async function (e) {
      e.preventDefault();
      var base = PRESET_ABILITIES[preset] || {};
      var edits = {};
      ABILITY_FIELDS.forEach(function (f) {
        if (values[f.key] !== base[f.key]) edits[f.key] = values[f.key];
      });
      save.disabled = true;
      var r = await api('PUT', '/members/' + encodeURIComponent(m.username), {
        displayName: display.value.trim(),
        preset: preset,
        edits: edits,
        sharedLibraries: libraries.value()
      });
      save.disabled = false;
      if (!r.ok) { error.textContent = F.errorOf(r); return; }
      F.closeSheet();
      F.toast('Saved');
      refreshFamily();
    } },
    el('label', { class: 'fam-label', for: 'edit-name' }, 'Name', display),
    el('div', { class: 'fam-label' }, 'Start from a plan', segmented(presets, preset, function (v) {
      preset = v;
      presetHelp.textContent = PRESETS[v];
      values = Object.assign({}, PRESET_ABILITIES[v]);
      drawFields();
    }, 'Plan'), presetHelp),
    fields,
    el('div', { class: 'fam-group' }, el('h3', { class: 'fam-group-title' }, 'Shared music they see'), libraries, sharedWarning),
    el('p', { class: 'fam-help' }, 'Outside songs stream at about 128 to 160 kbps whatever the limits say.'),
    error);
    F.openSheet('Edit ' + m.displayName, form, { side: true, sub: 'Choose a plan, then change any ability. Each one says what it does.', foot: [el('button', { class: 'btn btn-ghost', type: 'button', onclick: F.closeSheet }, 'Cancel'), save] });
  }

  function removeSheet(m) {
    var files = 'keep';
    var keep = false;
    var keepSwitch = switchControl(false, 'Keep their account and password', function () { keep = keepSwitch.input.checked; });
    var confirmInput = el('input', { class: 'set-input', id: 'confirm-name', autocapitalize: 'none', spellcheck: 'false', autocomplete: 'off' });
    var error = el('p', { class: 'field-error', role: 'alert' });
    var button = el('button', { class: 'btn btn-primary btn-destructive', type: 'submit', form: 'remove-member', disabled: true }, 'Remove ' + m.displayName);
    confirmInput.addEventListener('input', function () { button.disabled = confirmInput.value.trim().toLowerCase() !== m.username.toLowerCase(); });
    var choices = [
      ['keep', 'Keep them aside', 'Songs only they had are kept in a folder for you.'],
      ['promote', 'Add to shared', 'Everyone can play them.'],
      ['delete', 'Delete them', 'Songs only they had go to Octo’s trash.']
    ];
    var help = el('p', { class: 'fam-help' }, choices[0][2]);
    var form = el('form', { class: 'fam-form', id: 'remove-member', onsubmit: async function (e) {
      e.preventDefault();
      button.disabled = true;
      var r = await api('DELETE', '/members/' + encodeURIComponent(m.username), { confirm: confirmInput.value.trim(), files: files, keepAccount: keep });
      button.disabled = false;
      if (!r.ok) { error.textContent = F.errorOf(r); return; }
      F.closeSheet();
      F.toast(m.displayName + ' was removed');
      refreshFamily();
    } },
    el('div', { class: 'fam-label' }, 'Their songs', segmented(choices.map(function (c) { return [c[0], c[1]]; }), files, function (v) {
      files = v;
      help.textContent = choices.filter(function (c) { return c[0] === v; })[0][2];
    }, 'Their songs'), help),
    rowsCard([setRow('Keep their account and password', 'They can still sign in to Navidrome with the password they chose, but lose their family settings.', keepSwitch.node)]),
    el('label', { class: 'fam-label', for: 'confirm-name' }, 'Type ' + m.username + ' to confirm', confirmInput),
    error);
    F.openSheet('Remove ' + m.displayName + '?', form, { sub: 'Shared music is not touched. Unless you keep their account, their sign-in goes too.', foot: [el('button', { class: 'btn btn-ghost', type: 'button', onclick: F.closeSheet }, 'Cancel'), button] });
    confirmInput.focus();
  }

  // ── Family: requests to approve, all devices, playlists, activity ────────────────────
  async function viewInbox(main) {
    var filter = (/[?&]state=([A-Za-z]+)/.exec(location.hash) || [])[1] || 'Pending';
    var replies = await Promise.all([
      api('GET', '/requests' + (filter === 'All' ? '' : '?state=' + filter)),
      session.manager ? api('GET', '/members') : Promise.resolve({ ok: false })
    ]);
    if (!replies[0].ok) return failed(main, replies[0]);
    var members = replies[1].ok ? replies[1].data : [];
    var fullFor = function (request) {
      var m = members.filter(function (x) { return x.username === request.username; })[0];
      if (!m || !m.storageLimitGb) return null;
      var limit = m.storageLimitGb * 1e9;
      if (m.storageUsedBytes < limit) return null;
      return 'Their library is full (' + F.bytes(m.storageUsedBytes) + ' of ' + m.storageLimitGb + ' GB). Approving adds more anyway.';
    };
    var list = el('ul', { class: 'fam-list' });
    replies[0].data.forEach(function (request) {
      list.appendChild(requestRow(request, { fullFor: fullFor, decide: async function (r, approve, note) {
        var answer = await api('POST', '/requests/' + encodeURIComponent(r.id) + (approve ? '/approve' : '/decline'), { note: note });
        if (!answer.ok) return F.toast(F.errorOf(answer), true);
        F.toast(approve ? 'Approved' : 'Declined');
        session.pending = Math.max(0, (session.pending || 1) - 1);
        route();
      } }));
    });
    var filters = segmented([['Pending', 'Waiting'], ['Approved', 'Approved'], ['Done', 'Done'], ['Declined', 'Declined'], ['Failed', 'Failed'], ['All', 'All']], filter, function (v) {
      location.hash = '#/inbox?state=' + v;
    }, 'Show');
    F.clear(main);
    F.append(main, [
      pageHeader('checkCircle', 'Requests to approve', 'Copies family members asked for. Approve or decline, with a note if you like; they hear what you decide.'),
      el('div', { class: 'fam-toolbar' }, filters),
      card(null, null, null, replies[0].data.length ? list
        : empty('checkCircle', filter === 'Pending' ? 'Nothing is waiting for you. New requests show up here, and in your notifications.' : 'Nothing here.'))
    ]);
  }

  async function viewAllDevices(main) {
    var replies = await Promise.all([api('GET', '/devices'), api('GET', '/members')]);
    if (!replies[0].ok) return failed(main, replies[0]);
    var members = {};
    (replies[1].ok ? replies[1].data : []).forEach(function (m) { members[m.username.toLowerCase()] = true; });
    var list = el('ul', { class: 'fam-list' });
    replies[0].data.forEach(function (d) {
      var member = members[String(d.username || '').toLowerCase()];
      list.appendChild(deviceRow(d, { owner: true, forget: member ? '/devices/' : null, after: function () { viewAllDevices(main); } }));
    });
    F.clear(main);
    F.append(main, [
      pageHeader('plugs', 'All devices', 'Every family member’s devices, and the apps Octo has seen for everyone else. Octo adds them as they sign in.'),
      card(null, null, null, replies[0].data.length ? list : empty('plugs', 'No devices yet. They show up here as members sign in on their apps.', el('a', { class: 'btn', href: '#/members' }, 'Open members'))),
      el('p', { class: 'fam-help fam-gap' }, 'Remove from list only tidies the list. To sign a member out everywhere, reset their password on Members.')
    ]);
  }

  async function viewPlaylists(main) {
    var reply = await api('GET', '/playlists');
    if (!reply.ok) return failed(main, reply);
    var list = el('ul', { class: 'fam-list' });
    reply.data.forEach(function (p) {
      var mine = p.createdBy && p.createdBy.toLowerCase() === session.username.toLowerCase();
      var side = el('div', { class: 'fam-row-side' });
      if (session.manager) {
        var approve = switchControl(p.approveAdditions, 'Approve additions to ' + p.name, async function () {
          var r = await api('PUT', '/playlists/' + encodeURIComponent(p.id), { approveAdditions: approve.input.checked });
          if (!r.ok) { approve.input.checked = !approve.input.checked; return F.toast(F.errorOf(r), true); }
          F.toast(approve.input.checked ? 'Additions wait for approval' : 'Anyone can add');
        });
        side.appendChild(el('span', { class: 'fam-inline-switch' }, el('span', { class: 'fam-row-meta' }, 'Approve additions'), approve.node));
      }
      if (session.manager || mine) {
        side.appendChild(el('button', { class: 'btn btn-sm btn-ghost', type: 'button', onclick: async function () {
          var name = window.prompt('New name', p.name);
          if (!name || !name.trim()) return;
          var r = await api('PUT', '/playlists/' + encodeURIComponent(p.id), { name: name.trim() });
          if (!r.ok) return F.toast(F.errorOf(r), true);
          viewPlaylists(main);
        } }, 'Rename'));
      }
      if (session.manager) {
        side.appendChild(el('button', { class: 'btn btn-sm btn-ghost btn-danger', type: 'button', onclick: async function () {
          if (!confirmIn('Delete the family playlist ' + p.name + '? The songs stay in the library.')) return;
          var r = await api('DELETE', '/playlists/' + encodeURIComponent(p.id));
          if (!r.ok) return F.toast(F.errorOf(r), true);
          viewPlaylists(main);
        } }, 'Delete'));
      }
      list.appendChild(el('li', { class: 'fam-row stack' },
        el('span', { class: 'fam-row-ico', 'aria-hidden': 'true' }, F.icon('playlist')),
        el('div', { class: 'fam-row-main' },
          el('div', { class: 'fam-row-title' }, p.name),
          el('div', { class: 'fam-row-meta' }, p.songCount + (p.songCount === 1 ? ' song' : ' songs') + ' · made by ' + p.createdBy)),
        side));
    });
    var name = el('input', { class: 'set-input', placeholder: 'Road trip', 'aria-label': 'Playlist name' });
    var canEdit = !session.abilities || session.abilities.familyPlaylistsEdit;
    var create = canEdit ? el('form', { class: 'set-inline fam-create', onsubmit: async function (e) {
      e.preventDefault();
      if (!name.value.trim()) { name.focus(); return; }
      var r = await api('POST', '/playlists', { name: name.value.trim() });
      if (!r.ok) return F.toast(F.errorOf(r), true);
      F.toast('Made ' + r.data.name);
      viewPlaylists(main);
    } }, name, el('button', { class: 'btn btn-primary', type: 'submit' }, F.icon('plus'), 'Make a playlist')) : null;
    F.clear(main);
    F.append(main, [
      pageHeader('playlist', 'Family playlists', 'Playlists everyone in the family sees and can add to from any app.'),
      card(null, null, null, create,
        reply.data.length ? list : empty('playlist', canEdit ? 'No family playlists yet. Name one above, then add songs to it from any app.' : 'No family playlists yet.'))
    ]);
  }

  function stat(n, words) {
    return el('div', { class: 'fam-stat' }, el('span', { class: 'fam-stat-n' }, n), el('span', { class: 'fam-row-meta' }, words));
  }

  async function viewActivity(main) {
    var reply = await api('GET', '/load');
    if (!reply.ok) return failed(main, reply);
    var load = reply.data;
    var streams = el('ul', { class: 'fam-list' });
    load.streams.forEach(function (s) {
      streams.appendChild(el('li', { class: 'fam-row' },
        el('span', { class: 'fam-row-ico', 'aria-hidden': 'true' }, F.icon('radio')),
        el('div', { class: 'fam-row-main' },
          el('div', { class: 'fam-row-title' }, s.songTitle || 'A song'),
          el('div', { class: 'fam-row-meta' }, s.username + ' · ' + s.device)),
        el('div', { class: 'fam-row-side' }, placeBadge(s.place), badge(s.kbps ? s.kbps + ' kbps' : 'Original'))));
    });
    var queue = el('ul', { class: 'fam-list' });
    load.queue.forEach(function (q) {
      queue.appendChild(el('li', { class: 'fam-row' }, el('div', { class: 'fam-row-main' }, el('div', { class: 'fam-row-title' }, q.username)), el('div', { class: 'fam-row-side' }, badge(q.waiting + ' waiting'))));
    });
    // The helper's slots, or a plain note when it did not answer (never zeros it did not say).
    var shim = load.shim;
    var helper = shim
      ? [stat(String(shim.active), 'Outside songs streaming'), stat(shim.limit ? shim.ytdlpRunning + ' of ' + shim.limit : String(shim.ytdlpRunning), 'Lookups running')]
      : el('div', { class: 'fam-stat' }, el('strong', null, 'The YouTube helper did not answer'), el('span', { class: 'fam-row-meta' }, 'Outside songs and lookups'));
    F.clear(main);
    F.append(main, [
      pageHeader('drives', 'Activity', 'How busy the server is right now: who is playing, what is downloading, and how much is going out.'),
      card(null, null, null, el('div', { class: 'fam-stats' },
        stat(String(load.streams.length), 'Streams playing'),
        helper,
        stat(load.uploadKbps + ' kbps', 'Sent out now'))),
      el('div', { class: 'fam-grid two' },
        card('Playing now', null, null, load.streams.length ? streams : empty('radio', 'Nothing is playing right now.')),
        card('Downloads waiting', null, null, load.queue.length ? queue : empty('tray', 'No downloads waiting.')))
    ]);
  }

  // ── The invite popup ─────────────────────────────────────────────────────────────────
  // Who it is for and how long the link works at the top; where they will use it (anywhere or
  // at home); the QR code on a raised panel on the left, and the link and what happens next,
  // numbered, on the right (stacked on narrow screens); and a footer with Done.

  function doneButton() {
    return el('button', { class: 'btn btn-primary fam-done', type: 'button', onclick: F.closeSheet }, 'Done');
  }

  function footer(pop, left, right) {
    F.clear(pop.foot);
    F.append(pop.foot, [el('div', { class: 'fam-pop-foot-left' }, left), el('div', { class: 'fam-pop-foot-right' }, right)]);
  }

  function subLine(pop, parts, icon) {
    pop.sub.className = 'fam-pop-sub';
    F.clear(pop.sub);
    F.append(pop.sub, [el('span', { class: 'fam-sub-ico', 'aria-hidden': 'true' }, F.icon(icon || 'clock'))].concat(parts));
  }

  // A numbered way to use the link.
  function way(n, title, content) {
    return el('section', { class: 'fam-way' },
      el('h3', { class: 'fam-way-title' }, el('span', { class: 'fam-way-n', 'aria-hidden': 'true' }, String(n)), title),
      content);
  }

  // The QR code on its raised panel, with its step label and caption.
  function qrPanel(link, caption) {
    return el('figure', { class: 'fam-qr-panel' },
      el('h3', { class: 'fam-way-title' }, el('span', { class: 'fam-way-n', 'aria-hidden': 'true' }, '1'), 'Scan'),
      F.qrCard(link),
      el('figcaption', null, caption));
  }

  // A grey stand-in for the QR code and the link while Octo answers.
  function skeleton() {
    return el('div', { class: 'fam-pair', 'aria-busy': 'true' },
      el('div', { class: 'fam-qr-panel', 'aria-hidden': 'true' },
        el('span', { class: 'fam-skel fam-skel-label', style: 'width:70px;height:12px' }),
        el('div', { class: 'fam-qr-tile fam-skel' }),
        el('span', { class: 'fam-skel', style: 'width:150px;height:12px' })),
      el('div', { class: 'fam-pair-info', 'aria-hidden': 'true' },
        el('div', { class: 'fam-way' }, el('span', { class: 'fam-skel', style: 'width:180px;height:12px' }), el('div', { class: 'fam-skel fam-skel-row' })),
        el('div', { class: 'fam-way' }, el('span', { class: 'fam-skel', style: 'width:220px;height:12px' }), el('span', { class: 'fam-skel', style: 'width:200px;height:12px' }))),
      el('p', { class: 'visually-hidden' }, 'Loading'));
  }

  // One plain line and a retry button, with Octo's reason under it when it gave one.
  function errorLine(line, reply, retry) {
    var reason = reply && reply.data && reply.data.error;
    return el('div', { class: 'fam-pop-error', role: 'alert' },
      el('span', { class: 'fam-pop-error-ico' }, F.icon('warning')),
      el('div', { class: 'fam-pop-error-text' },
        el('p', { class: 'fam-pop-error-line' }, line),
        reason ? el('p', { class: 'fam-help' }, reason) : null),
      el('button', { class: 'btn btn-sm', type: 'button', onclick: retry }, F.icon('retry'), 'Retry'));
  }

  function hostOf(url) {
    try { return new URL(url).host; } catch (e) { return url || ''; }
  }

  var whereCount = 0;

  // Where the link or address is for: two equal options, Anywhere and At home, the chosen one
  // filled with a check, and one line under them saying what it uses. Anywhere is hidden for
  // someone who may not listen away; without an outside address it stays choosable, marked
  // with an info icon, and asks for one.
  function linkChoice(answer, choice, onPick, forName) {
    var homeBase = (answer.servers && answer.servers.home) || (answer.links && answer.links.home) || answer.server || '';
    var uses = el('p', { class: 'fam-where-uses' },
      F.icon(choice === 'anywhere' ? 'globe' : 'house'),
      el('span', null, choice === 'anywhere'
        ? (answer.anywhereAvailable ? 'Uses ' + answer.publicUrl : 'Needs your outside address first')
        : 'Uses your home network (' + hostOf(homeBase) + ')'));
    if (answer.awayAllowed === false) {
      return el('div', { class: 'fam-where-wrap' },
        el('p', { class: 'fam-choice-note' }, F.icon('house'),
          el('span', null, 'At home only: ' + (forName || 'this account') + ' does not listen away from home.')),
        uses);
    }
    var id = 'fam-where-' + (++whereCount);
    var options = [['anywhere', 'Anywhere', 'globe'], ['home', 'At home', 'house']];
    var buttons = [];
    var group = el('div', { class: 'fam-where', role: 'radiogroup', 'aria-labelledby': id });
    options.forEach(function (option, i) {
      var on = option[0] === choice;
      var missing = option[0] === 'anywhere' && !answer.anywhereAvailable;
      var button = el('button', {
        type: 'button',
        class: 'fam-where-opt' + (on ? ' on' : ''),
        role: 'radio',
        'aria-checked': on ? 'true' : 'false',
        tabindex: on ? '0' : '-1',
        onclick: function () { if (!on) onPick(option[0]); },
        onkeydown: function (e) {
          var step = e.key === 'ArrowRight' || e.key === 'ArrowDown' ? 1 : e.key === 'ArrowLeft' || e.key === 'ArrowUp' ? -1 : 0;
          if (!step) return;
          e.preventDefault();
          var next = options[(i + step + options.length) % options.length][0];
          onPick(next);
          requestAnimationFrame(function () {
            var again = document.querySelector('dialog[open] .fam-where-opt[aria-checked="true"]');
            if (again) again.focus();
          });
        }
      },
      F.icon(option[2]),
      el('span', { class: 'fam-where-text' }, option[1]),
      missing ? el('span', { class: 'fam-where-info', title: 'Needs your outside address' }, F.icon('info'), el('span', { class: 'visually-hidden' }, ', needs your outside address')) : null,
      el('span', { class: 'fam-where-check', 'aria-hidden': 'true' }, F.icon('check')));
      buttons.push(button);
      group.appendChild(button);
    });
    return el('div', { class: 'fam-where-wrap' },
      el('p', { class: 'fam-where-label', id: id }, forName ? 'Where will they use it?' : 'Where will you use it?'),
      group,
      uses);
  }

  // What the "At home only" choice, or a missing outside address, means, above the QR code.
  function homeNote(answer, choice, regenerate) {
    if (choice === 'anywhere' && !answer.anywhereAvailable) {
      return el('section', { class: 'fam-needs-address' },
        el('h3', { class: 'fam-needs-title' }, F.icon('globe'), 'Set your outside address first'),
        el('p', { class: 'fam-help' }, 'Links that work anywhere use the address people reach Octo at from outside, through your reverse proxy or tunnel.'),
        session.owner
          ? el('div', { class: 'fam-stack' },
            el('div', null, el('a', { class: 'btn btn-sm', href: STATUS_ADDRESS, target: '_blank', rel: 'noopener' }, F.icon('out'), 'Set it on the Status page')),
            addressField(function (url, words) { if (url && regenerate) regenerate(words); }))
          : el('p', { class: 'fam-help' }, 'Ask whoever runs this server to set its outside address.'));
    }
    return null;
  }

  function pickDefault(answer) {
    return answer.anywhereAvailable ? 'anywhere' : 'home';
  }

  // Reset password: after a plain confirmation, the member's password stops working at once and
  // the invite popup shows their new sign-in link.
  async function resetPassword(m) {
    if (!confirmIn('Reset ' + m.displayName + '’s password? The one they use now stops working at once, on every app. They get a new link to choose another.')) return;
    var r = await api('POST', '/members/' + encodeURIComponent(m.username) + '/reset', {});
    if (!r.ok) return F.toast(F.errorOf(r), true);
    inviteDialog(Object.assign({}, m, { needsPassword: false }), r.data, true);
    refreshFamily();
  }

  // Invite a member, or (`reset`) show a member's new sign-in link after a password reset.
  // `answer` is a link already in hand; without one the dialog asks for one. Send a new link
  // ends the one shown before it.
  function inviteDialog(m, answer, reset) {
    var name = m.displayName;
    var pop = F.openDialog({ wide: true, avatar: name });
    var done = doneButton();
    var turn = 0;
    var choice = null;
    var current = null;
    var flash = null;
    pop.title.textContent = reset ? 'New sign-in link for ' + name : 'Invite ' + name;
    var again = el('button', { class: 'btn btn-ghost', type: 'button', onclick: function () { load(); } }, F.icon('retry'), 'Send a new link');
    footer(pop, again, done);
    pop.onClose(function () { if (currentView() === 'overview') route(); });

    function lifetime(days) {
      days = Number(days) || 7;
      return 'The link works once and expires in ' + (days === 1 ? '1 day' : days + ' days');
    }

    function draw(data) {
      current = data;
      if (!choice || (choice === 'anywhere' && data.awayAllowed === false)) choice = pickDefault(data);
      subLine(pop, [lifetime(data.inviteDays)]);
      var link = (data.links && data.links[choice]) || data.inviteLink;
      var top = el('div', { class: 'fam-pop-top' },
        linkChoice(data, choice, function (v) { choice = v; draw(current); }, name),
        flash ? el('p', { class: 'inline-note ok' }, F.icon('check'), flash) : null);
      flash = null;
      var needs = homeNote(data, choice, function (words) { flash = words; load(); });
      F.clear(pop.body);
      if (choice === 'anywhere' && !data.anywhereAvailable) {
        F.append(pop.body, [top, needs]);
        return;
      }
      F.append(pop.body, [top, needs, el('div', { class: 'fam-pair' },
        qrPanel(link, name + ' scans this with their phone'),
        el('div', { class: 'fam-pair-info' },
          way(2, 'Or send ' + name + ' this link',
            F.linkRow(link, 'Copy the link', { title: 'Join my Octo family', text: 'Your sign-in link for Octo:' })),
          way(3, 'Then', el('p', { class: 'fam-pop-line' }, name + ' picks a password, then signs in on any app.'))))]);
    }

    async function load() {
      var mine = ++turn;
      again.disabled = true;
      pop.sub.textContent = 'Making a link';
      F.clear(pop.body).appendChild(skeleton());
      var r = await api('POST', '/members/' + encodeURIComponent(m.username) + (reset ? '/reset' : '/invite'), {});
      if (!pop.isOpen() || mine !== turn) return;
      again.disabled = false;
      if (!r.ok) {
        pop.sub.textContent = '';
        F.clear(pop.body).appendChild(errorLine('Couldn’t make a link. Try again.', r, load));
        return;
      }
      draw(r.data);
    }

    if (answer) draw(answer);
    else load();
    done.focus();
  }

  // ── Listening ────────────────────────────────────────────────────────────────────────
  // Drawn by /admin/listening.js, the dashboard's own Listening page: a member's plays, or
  // everyone's for the people who manage the family.
  var listeningPage = null;

  function mountListening(main, everyone) {
    var holder = el('div', { class: 'fam-listening' });
    F.clear(main).appendChild(holder);
    if (!window.OctoListening) {
      holder.appendChild(el('p', { class: 'notice notice-error', role: 'alert' }, 'Listening could not load. Reload the page to try again.'));
      return;
    }
    listeningPage = window.OctoListening.mount(holder, {
      title: everyone ? 'Everyone\u2019s listening' : 'Listening',
      canSeeEveryone: everyone,
      cover: function (item) {
        var id = item.coverId || item.songId;
        return id ? '/api/family/cover/' + encodeURIComponent(id) + '?size=160' : '';
      },
      fetch: async function (days, user) {
        var path = (everyone ? '/listening' : '/me/listening') + '?range=' + days;
        if (everyone && user) path += '&user=' + encodeURIComponent(user);
        var reply = await api('GET', path);
        return { ok: reply.ok, status: reply.status, data: reply.data, error: F.errorOf(reply) };
      }
    });
  }

  function viewListening(main) {
    mountListening(main, false);
  }

  function viewEveryoneListening(main) {
    mountListening(main, true);
  }

  // ── Routing ──────────────────────────────────────────────────────────────────────────
  var VIEWS = {
    overview: viewOverview,
    devices: viewDevices,
    requests: viewRequests,
    saved: viewSaved,
    library: viewLibrary,
    import: viewImport,
    ready: viewReady,
    quality: viewQuality,
    account: viewAccount,
    members: viewMembers,
    inbox: viewInbox,
    'all-devices': viewAllDevices,
    playlists: viewPlaylists,
    activity: viewActivity,
    listening: viewListening,
    'everyone-listening': viewEveryoneListening
  };

  function allowed(view) {
    var nav = navFor(session);
    return nav.you.concat(nav.family).indexOf(view) >= 0;
  }

  async function route() {
    if (!session) return;
    var view = currentView();
    if (!VIEWS[view] || !allowed(view)) view = defaultView(session);
    if (listeningPage) {
      listeningPage.destroy();
      listeningPage = null;
    }
    var main = frame(view);
    document.title = (VIEWS_INFO[view] ? VIEWS_INFO[view][0] + ' · ' : '') + 'Octo Family';
    await VIEWS[view](main);
    var heading = main.querySelector('h1');
    if (heading && (document.activeElement === document.body || !document.activeElement)) heading.focus({ preventScroll: true });
  }

  async function start() {
    var reply = await api('GET', '/session');
    if (reply.status === 401) return renderSignIn();
    if (!reply.ok) return renderSignIn(F.errorOf(reply));
    session = reply.data;
    session.pending = 0;
    if (session.familyEnabled) {
      var me = await api('GET', '/me');
      if (me.ok) session.abilities = me.data.abilities;
      if (session.approver) {
        var waiting = await api('GET', '/requests?state=Pending');
        if (waiting.ok) session.pending = waiting.data.length;
      }
    }
    route();
  }

  // The dashboard's "Open Family" hands its admin over with a single-use token in the
  // fragment: it is exchanged for the page's own sign-in, then cleared from the address bar.
  async function handoff() {
    var match = /[#&]handoff=([^&]+)/.exec(location.hash);
    if (!match) return;
    history.replaceState(null, '', location.pathname + location.search);
    var reply = await api('POST', '/handoff', { token: decodeURIComponent(match[1]) });
    if (!reply.ok && reply.status !== 401) F.toast(F.errorOf(reply), true);
  }

  // An invite link (or an old pairing link) opened here goes to the join page.
  if (/invite=/.test(location.hash) || /[#&]c=\d/.test(location.hash)) {
    location.replace('/family/join' + location.hash);
    return;
  }
  window.addEventListener('hashchange', function () { F.closeSheet(); closeDrawer(); route(); });
  handoff().then(start);
})();
