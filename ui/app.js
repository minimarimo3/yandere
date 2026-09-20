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
  $('cfg-groq-key').value = c.groq_api_key;
  $('cfg-gemini-key').value = c.gemini_api_key;
  $('cfg-interval').value = c.observation_interval_seconds;
  $('cfg-gap').value = c.minimum_notification_gap_minutes;
  $('cfg-persona').value = c.persona;
}
function renderBootstrap(d) {
  data = d; fillConfig(d.config); renderMessages(d.messages);
  const sleeping = Boolean(d.rhythm?.sleeping);
  $('presence-label').textContent = sleeping ? 'MACの中で寝ている' : 'MACの中にいる';

  if (sleeping) {
    $('status-dot').className = 'status sleeping';
    $('status-dot').title = `${d.config.companion_name}は睡眠中。起床予定 ${d.rhythm.wake_at_display}`;
    $('observation').textContent = `すやすや寝ています。`;
  } else {
    $('status-dot').className = `status ${d.screenpipe_ok ? 'good' : 'bad'}`;
    $('status-dot').title = d.screenpipe_ok ? 'screenpipe daemon 稼働中' : 'screenpipe daemon に接続できません';
    if (d.last_observation) {
      const o = d.last_observation.decision;
      $('observation').textContent = `${o.working ? '作業中' : '作業外'} · ${Math.round(o.focus_level * 100)}% — ${o.summary}`;
    } else {
      $('observation').textContent = 'まだ観察を始めていません。';
    }
  }

  $('chat-input').disabled = sleeping;
  $('send').disabled = sleeping;
  $('observe-now').disabled = sleeping;
  $('make-diary').disabled = sleeping;
  $('chat-input').placeholder = sleeping ? `${d.config.companion_name}は寝ています…` : '話しかける…';

  if (d.today_diary) $('diary-text').textContent = d.today_diary.text;
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

$('save-settings').addEventListener('click', async () => {
  const c = {...data.config,
    companion_name:$('cfg-companion-name').value.trim() || '美月',
    user_name:$('cfg-user-name').value.trim() || 'あなた',
    screenpipe_api_key:$('cfg-screenpipe-key').value.trim(),
    groq_api_key:$('cfg-groq-key').value.trim(),
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

listen('new_message', async () => { await reload(); });
listen('observation', async () => { await reload(); });
listen('diary_ready', async () => { await reload(); });
listen('rhythm_changed', async () => { await reload(); });
reload().catch(e => toast(`起動エラー: ${e}`));
