const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
let data = null;

const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#039;'}[c]));

function toast(text) {
  const el = $('toast'); el.textContent = text; el.classList.add('show');
  setTimeout(() => el.classList.remove('show'), 2300);
}
function renderMessages(messages) {
  const el = $('messages');
  el.innerHTML = messages.map(m => `<div class="msg ${m.role}">${esc(m.text)}<span class="time">${new Date(m.created_at).toLocaleTimeString([], {hour:'2-digit',minute:'2-digit'})}${m.proactive ? ' · 自分から' : ''}</span></div>`).join('');
  el.scrollTop = el.scrollHeight;
}
function fillConfig(c) {
  $('companion-name').textContent = c.companion_name;
  $('cfg-companion-name').value = c.companion_name;
  $('cfg-user-name').value = c.user_name;
  $('cfg-screenpipe-key').value = c.screenpipe_api_key || '';
  $('cfg-gemini-key').value = c.gemini_api_key;
  $('cfg-interval').value = c.observation_interval_seconds;
  $('cfg-gap').value = c.minimum_notification_gap_minutes;
  $('cfg-persona').value = c.persona;
}
function setHealthBadge(id, ok, goodText, badText) {
  const el = $(id);
  el.className = `health-badge ${ok ? 'good' : 'bad'}`;
  el.textContent = ok ? goodText : badText;
}
function renderScreenpipeHealth(h, sleeping = false) {
  if (sleeping) {
    ['health-daemon','health-screen','health-accessibility','health-input','health-recorder'].forEach(id => {
      const el = $(id);
      el.className = 'health-badge pending';
      el.textContent = '睡眠中';
    });
    $('health-detail').textContent = h?.detail || '睡眠中のため状態確認を停止しています。';
    return;
  }

  h = h || {};
  setHealthBadge('health-daemon', Boolean(h.reachable), '接続中', '未接続');
  setHealthBadge('health-screen', Boolean(h.screen_capture_ok), '正常', h.vision_reason || h.frame_status || '要確認');
  setHealthBadge('health-accessibility', Boolean(h.accessibility_ok), '許可済み', '要許可');
  setHealthBadge('health-input', Boolean(h.input_monitoring_ok), '許可済み', '要許可');

  const recorder = $('health-recorder');
  recorder.className = `health-badge ${h.ui_recorder_running ? 'good' : 'bad'}`;
  recorder.textContent = h.ui_recorder_running
    ? `稼働中 · ${Number(h.events_inserted || 0).toLocaleString()}件`
    : '停止中';

  const bits = [];
  if (h.screenpipe_version) bits.push(`screenpipe ${h.screenpipe_version}`);
  if (h.ui_mode) bits.push(`UI mode: ${h.ui_mode}`);
  if (h.detail) bits.push(h.detail);
  if (!h.accessibility_ok || !h.input_monitoring_ok) {
    bits.push('権限変更後は美月とscreenpipeを再起動してください。');
  }
  $('health-detail').textContent = bits.join(' · ') || '状態を取得しました。';
}
function renderBootstrap(d) {
  data = d; fillConfig(d.config); renderMessages(d.messages);
  const sleeping = Boolean(d.rhythm?.sleeping);
  $('presence-label').textContent = sleeping ? 'MACの中で寝ている' : 'MACの中にいる';

  if (sleeping) {
    $('status-dot').className = 'status sleeping';
    $('status-dot').title = `${d.config.companion_name}は睡眠中`;
    $('observation').textContent = 'すやすや寝ています。';
  } else {
    const h = d.screenpipe_health || {};
    const fullyHealthy = Boolean(d.screenpipe_ok);
    const reachable = Boolean(h.reachable);
    $('status-dot').className = `status ${fullyHealthy ? 'good' : (reachable ? 'warn' : 'bad')}`;
    $('status-dot').title = fullyHealthy
      ? 'screenpipe / 入力監視ともに正常'
      : (reachable ? 'screenpipeは動作中ですが権限または入力監視に問題があります' : 'screenpipe daemon に接続できません');
    if (d.last_observation) {
      const o = d.last_observation.decision;
      const date = new Date(d.last_observation.created_at);
      const now = new Date();

      const sameDay =
        date.getFullYear() === now.getFullYear() &&
        date.getMonth() === now.getMonth() &&
        date.getDate() === now.getDate();

      const observedAt = sameDay
        ? date.toLocaleTimeString([], {
          hour: '2-digit',
          minute: '2-digit'
        })
        : date.toLocaleString([], {
          month: 'numeric',
          day: 'numeric',
          hour: '2-digit',
          minute: '2-digit'
        });

      $('observation').textContent =
        `${observedAt} · ${o.working ? '作業中' : '作業外'} · ${Math.round(o.focus_level * 100)}% — ${o.summary}`;
    } else {
      $('observation').textContent = 'まだ観察を始めていません。';
    }
  }

  $('chat-input').disabled = sleeping;
  $('send').disabled = sleeping;
  $('observe-now').disabled = sleeping;
  $('make-diary').disabled = sleeping;
  $('chat-input').placeholder = sleeping ? `${d.config.companion_name}は寝ています…` : '話しかける…';
  renderScreenpipeHealth(d.screenpipe_health, sleeping);

  if (d.today_diary) $('diary-text').textContent = d.today_diary.text;
  $('app-version').textContent = d.app_version || '-';
  $('app-log-path').textContent = d.log_path || '-';
  $('screenpipe-log-path').textContent = d.screenpipe_log_path || '-';
}
async function reload() { renderBootstrap(await invoke('bootstrap')); }

document.querySelectorAll('.tab').forEach(btn => btn.addEventListener('click', () => {
  document.querySelectorAll('.tab').forEach(b => b.classList.toggle('active', b === btn));
  document.querySelectorAll('.panel').forEach(p => p.classList.toggle('active', p.id === btn.dataset.tab));
}));

$('chat-form').addEventListener('submit', async (e) => {
  e.preventDefault(); const input = $('chat-input'); const text = input.value.trim(); if (!text) return;
  input.value = ''; $('send').disabled = true;
  data.messages.push({role:'user', text, created_at:new Date().toISOString(), proactive:false}); renderMessages(data.messages);
  try { const reply = await invoke('send_message', { text }); data.messages.push(reply); renderMessages(data.messages); }
  catch (e) { await reload(); toast(`会話エラー: ${e}`); }
  finally { $('send').disabled = Boolean(data?.rhythm?.sleeping); input.focus(); }
});

$('refresh-health').addEventListener('click', async () => {
  const b = $('refresh-health');
  b.disabled = true;
  b.textContent = '確認中…';
  try {
    const h = await invoke('check_screenpipe_health');
    if (data) data.screenpipe_health = h;
    renderScreenpipeHealth(h, Boolean(data?.rhythm?.sleeping));
    toast('screenpipeの状態を更新しました');
  } catch(e) {
    toast(`ヘルス確認エラー: ${e}`);
  } finally {
    b.disabled = false;
    b.textContent = '再チェック';
  }
});

$('save-settings').addEventListener('click', async () => {
  const c = {...data.config,
    companion_name:$('cfg-companion-name').value.trim() || '美月',
    user_name:$('cfg-user-name').value.trim() || 'あなた',
    screenpipe_api_key:$('cfg-screenpipe-key').value.trim(),
    gemini_api_key:$('cfg-gemini-key').value.trim(),
    observation_interval_seconds:Number($('cfg-interval').value) || 300,
    minimum_notification_gap_minutes:Number($('cfg-gap').value) || 20,
    persona:$('cfg-persona').value
  };
  try { await invoke('save_config', { newConfig:c }); data.config = c; fillConfig(c); toast('保存しました'); }
  catch(e) { toast(`保存エラー: ${e}`); }
});

$('make-diary').addEventListener('click', async () => {
  const b=$('make-diary'); b.disabled=true; b.textContent='書いてる…';
  try { const d=await invoke('generate_diary'); $('diary-text').textContent=d.text; toast('今日の日記を書きました'); }
  catch(e) { toast(`日記エラー: ${e}`); }
  finally { b.disabled=Boolean(data?.rhythm?.sleeping); b.textContent='今つくる'; }
});

$('observe-now').addEventListener('click', async () => {
  const b=$('observe-now'); b.disabled=true;
  try { await invoke('observe_now'); await reload(); toast('観察しました'); }
  catch(e) { toast(`観察エラー: ${e}`); }
  finally { b.disabled=Boolean(data?.rhythm?.sleeping); }
});


let quitArmedUntil = 0;
let quitResetTimer = null;

$('quit-all').addEventListener('click', async () => {
  const b = $('quit-all');
  const now = Date.now();

  // Native confirm()/alert() dialogs are unreliable from a non-activating
  // NSPanel (especially while another app owns the fullscreen Space).
  // Use an inline two-step confirmation instead.
  if (now > quitArmedUntil) {
    quitArmedUntil = now + 5000;
    b.textContent = 'もう一度押すと終了';
    toast('5秒以内にもう一度押すと、美月とscreenpipeを終了します');
    if (quitResetTimer) clearTimeout(quitResetTimer);
    quitResetTimer = setTimeout(() => {
      quitArmedUntil = 0;
      b.textContent = '美月とscreenpipeを終了';
    }, 5000);
    return;
  }

  quitArmedUntil = 0;
  if (quitResetTimer) clearTimeout(quitResetTimer);
  b.disabled = true;
  b.textContent = '終了しています…';
  try {
    await invoke('quit_all');
  } catch(e) {
    b.disabled = false;
    b.textContent = '美月とscreenpipeを終了';
    toast(`終了エラー: ${e}`);
  }
});

listen('new_message', async () => { await reload(); });
listen('observation', async () => { await reload(); });
listen('diary_ready', async () => { await reload(); });
listen('rhythm_changed', async () => { await reload(); });
reload().catch(e => toast(`起動エラー: ${e}`));
