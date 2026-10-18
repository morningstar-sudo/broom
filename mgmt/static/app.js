// Bootstrap colour mode follows the OS (light / dark), also when it changes while the page is open.
const darkQ=matchMedia('(prefers-color-scheme: dark)');
const setTheme=()=>document.documentElement.dataset.bsTheme=darkQ.matches?'dark':'light';
setTheme();darkQ.addEventListener('change',setTheme);
async function j(url,opt){const r=await fetch(url,opt);
  if(r.status===401){location.replace('/login');throw new Error('login required');}
  if(!r.ok)throw new Error(await r.text());return r.json()}
const esc=s=>(s==null?'':String(s)).replace(/[&<>"'`]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;','`':'&#96;'}[c]));
// Icons = symbols of /icons.svg, drawn in the text colour (class n colours ok / err / wait / warn; '' = none).
const ICON_LABEL={ok:'ok',err:'error',wait:'working',warn:'warning'};
const ic=(n,c)=>`<svg class="ic ${c??n}" ${ICON_LABEL[n]?`role="img" aria-label="${ICON_LABEL[n]}"`:'aria-hidden="true"'}><use href="/icons.svg#${n}"/></svg>`;
// Status text keeps its ✓ / ✗ / ⏳ marks (the server's job statuses start with them, and code checks them):
// say() shows it in el, escaped, with the marks drawn as icons.
const MARK={'✓':'ok','✗':'err','⏳':'wait'};
function say(el,t){if(el)el.innerHTML=esc(t).replace(/[✓✗⏳]/g,c=>ic(MARK[c]));}
// Badges used in several tables: machine on/off, image OS.
const led=on=>`<span class="badge ${on?'text-bg-success':'text-bg-secondary'}">${on?'ON':'OFF'}</span>`;
const osPill=os=>`<span class="badge rounded-pill ${os==='linux'?'text-bg-success':'text-bg-info'}">${esc(os)}</span>`;
// No data in inline JS (onclick="f('${esc(x)}')" is unsafe: the browser decodes &#39; back to ' before running
// it; the CSP blocks inline handlers anyway). Elements name an ACTIONS entry in data-click / data-change /
// data-input and carry JSON args in data-args; one listener per event type calls ACTIONS[name](...args, el, event).
const act=(name,...args)=>`data-click="${name}" data-args="${esc(JSON.stringify(args))}"`;
for(const ev of ['click','change','input'])document.addEventListener(ev,e=>{
  const el=e.target.closest&&e.target.closest('[data-'+ev+']');if(!el)return;
  const f=ACTIONS[el.dataset[ev]];if(f)f(...JSON.parse(el.dataset.args||'[]'),el,e);
});

// ---- tabs: load HTML fragments on demand (/ui/<page>) + only load data for the tab being viewed ----
const loaders={machines:loadStatus,remote:loadRemote,server:loadServer,devices:loadDevices,groups:loadGroups,images:loadImages,games:loadGames,drivers:loadDrivers,network:loadDhcp,windows:loadCafe,system:()=>{loadCafe();loadUsers();},benchmark:loadBench,logs:loadLogs};
const fragCache={};
let curPage=null;
async function showPage(p){
  if(iscsiES&&p!=='server'){iscsiES.close();iscsiES=null;}
  if(p!=='remote'){if(rmWs){rmWs.onclose=null;rmWs.close();rmWs=null;rmCtx=null;}rmCloseDec();closeWall();}
  document.querySelectorAll('nav a').forEach(x=>x.classList.toggle('active',x.dataset.page===p));
  const grp=document.querySelector(`nav a[data-page="${p}"]`)?.closest('details');if(grp)grp.open=true; // its group shows
  if(fragCache[p]===undefined){
    try{const r=await fetch('/ui/'+p);
      if(r.status===401){location.replace('/login');return;} // session expired: never cache the "login required" text
      if(!r.ok)throw new Error(r.status);
      fragCache[p]=await r.text();}
    catch(e){document.getElementById('main').innerHTML='<p class="form-text">failed to load page '+p+'</p>';return;}
  }
  document.getElementById('main').innerHTML=fragCache[p];
  curPage=p;
  if(loaders[p])loaders[p]();
}
// Page from the URL (/machines, /images, ...); "/" or unknown → machines.
const pageFromPath=()=>{const p=location.pathname.slice(1);return loaders[p]?p:'machines';};
document.querySelectorAll('nav a').forEach(a=>a.onclick=e=>{
  if(e.ctrlKey||e.metaKey||e.shiftKey||e.button!==0)return;   // new tab/window: let the browser handle it
  e.preventDefault();
  bootstrap.Offcanvas.getInstance('#sidebar')?.hide();   // small screens: close the slid-in menu
  if(a.dataset.page!==curPage)history.pushState(null,'','/'+a.dataset.page);
  showPage(a.dataset.page);
});
window.onpopstate=()=>showPage(pageFromPath());
// Menu groups: folded by default, each one's open/closed remembered in this browser (localStorage may be off: then
// everything just starts folded, the current page's group open).
document.querySelectorAll('nav details[data-group]').forEach(d=>{
  const k='navgrp:'+d.dataset.group;
  try{if(localStorage.getItem(k)==='1')d.open=true;}catch(e){}
  d.addEventListener('toggle',()=>{try{localStorage.setItem(k,d.open?'1':'0');}catch(e){}});
});

// ---- machines (filter + client-side paging) ----
let allMachines=[], mPage=1;
// Rows per page (Machines): 15 / 50 / 100 / all (0 → Infinity), remembered in this browser.
let M_PAGE=15;
try{const s=localStorage.getItem('m_psize');if(s!==null)M_PAGE=+s||Infinity;}catch(e){}
function mPageSize(){const n=+document.getElementById('m_psize').value;M_PAGE=n||Infinity;
  try{localStorage.setItem('m_psize',String(n));}catch(e){}mPage=1;renderMachines();}
async function loadStatus(){
  allMachines=await j('/api/status');
  const online=allMachines.filter(m=>m.online).length;
  document.getElementById('onlinecount').textContent=online+'/'+allMachines.length+' online';
  if(curPage==='machines')renderMachines();
}
// Per-column filters (the second header row of the machine table). Text = case-insensitive "contains".
const MF=['mf_host','mf_grp','mf_ip','mf_mac','mf_state','mf_lic','mf_reg'];
const mf=id=>((document.getElementById(id)||{}).value||'').trim().toLowerCase();
const licErr=m=>!!m.license_result&&!/activated successfully/i.test(m.license_result);
function filteredMachines(){
  const has=(v,q)=>!q||(v||'').toLowerCase().includes(q);
  const [host,grp,ip,mac,state,lic,reg]=MF.map(mf);
  return allMachines.filter(m=>has(m.hostname,host)&&has(m.grp,grp)&&has(m.ip,ip)&&has(m.mac,mac)
    &&(!state||(state==='on')===!!m.online)
    &&(!lic||(lic==='none'?!m.license_tail:lic==='error'?licErr(m):m.license_state===lic))
    &&(!reg||(reg==='registered')===!!m.registered));
}
function clearMFilters(){MF.forEach(id=>{const e=document.getElementById(id);if(e)e.value='';});mPage=1;renderMachines();}
function renderMachines(){
  const rows=filteredMachines(), pages=Math.max(1,Math.ceil(rows.length/M_PAGE));
  if(mPage>pages)mPage=pages; if(mPage<1)mPage=1;
  const page=rows.slice((mPage-1)*M_PAGE,mPage*M_PAGE);
  document.querySelector('#machines tbody').innerHTML=page.map(m=>
    `<tr>
       <td>${m.hostname?esc(m.hostname):'<span class="mono">—</span>'} ${m.registered?'':'<span class="badge rounded-pill text-bg-warning">new</span>'}</td>
       <td class="mono">${m.grp?esc(m.grp):'—'}</td>
       <td class="mono">${esc(m.ip)}</td><td class="mono">${esc(m.mac)}</td>
       <td>${led(m.online)}${m.not_reset?' <span class="badge text-bg-danger" title="Windows started from the SSD without a PXE boot at '+new Date(m.not_reset*1000).toLocaleString()+' — this session was NOT reset (cable out, server down, boot order changed?). Cleared by the next PXE boot.">not reset</span>':''}${m.ssd_low?' <span class="badge text-bg-danger" title="SSD almost full during the session: '+gbs(m.ssd_low[0])+' free at '+new Date(m.ssd_low[1]*1000).toLocaleString()+' (the guest wrote a lot; Windows → SSD room). Cleared by the next PXE boot.">SSD full</span>':''}${mxBadge(m.metrics)}${upBadge(m)}</td>
       <td class="mono" title="${esc(m.license_result||'')}">${licCell(m)}</td>
       <td><div class="d-flex gap-1">${m.registered
          ? `<button class="btn btn-sm btn-outline-secondary" ${act('wake',m.mac)}>Wake</button>
             <button class="btn btn-sm btn-outline-secondary" ${act('manage',m.id)} data-owner>Manage</button>
             ${m.remote?`<button class="btn btn-sm btn-outline-primary" ${act('remoteTo',m.hostname)} title="See and control this machine">Remote</button>`:''}`
          : `<button class="btn btn-sm btn-outline-primary" ${act('regMachine',m.mac,m.ip,m.hostname)} data-owner>Register</button>`}</div></td>
     </tr>`).join('') || '<tr><td colspan=7 class="mono">no matching machines</td></tr>';
  document.getElementById('m_count').textContent=rows.length+' machines';
  document.getElementById('m_page').textContent='Page '+mPage+'/'+pages;
  const ps=document.getElementById('m_psize');if(ps)ps.value=M_PAGE===Infinity?'0':String(M_PAGE);
}
function mPrev(){if(mPage>1){mPage--;renderMachines();}}
function mNext(){const pages=Math.max(1,Math.ceil(filteredMachines().length/M_PAGE));if(mPage<pages){mPage++;renderMachines();}}
// Windows computer name (NetBIOS). Keep in sync with hostname_ok() in machines.rs.
const HOST_RULE='Hostname: 1-15 characters, only letters/digits/"-", must not start or end with "-", not all digits';
const hostOk=h=>/^[A-Za-z0-9-]{1,15}$/.test(h)&&!/^-|-$/.test(h)&&!/^[0-9]+$/.test(h);
async function regMachine(mac,ip,host){
  let hostname=host||'PC';
  do{hostname=await ask('Hostname for '+mac,hostOk(hostname)?'':HOST_RULE,hostname);if(hostname===null)return;hostname=hostname.trim();}
  while(!hostOk(hostname));
  try{await j('/api/machines',mk({mac,ip:ip||null,hostname}));}catch(e){toast(e.message,true);return;}
  toast('Registered. Reboot the machine to get its IP/name (full DHCP mode).');refreshMachines();
}
// Whichever machine page is open reloads after a change (Machines = quick dashboard, Devices = management).
function refreshMachines(){return curPage==='devices'?loadDevices():loadStatus();}
// Machines → Devices page with this machine's detail open.
function manage(id){dvPending=id;history.pushState(null,'','/devices');showPage('devices');}
async function wake(mac){await j('/api/wake',mk({mac}));toast('WOL sent to '+mac)}
// Windows license key: handed out ONCE to the machine's IP when its base is rebuilt (next boot). The full key +
// activation result are shown to the logged-in operator. Keep keyOk in sync with key_ok() in license.rs.
const KEY_RULE='License key: 25 letters/digits as XXXXX-XXXXX-XXXXX-XXXXX-XXXXX (empty = remove)';
const keyOk=k=>/^[A-Z0-9]{5}(-[A-Z0-9]{5}){4}$/.test(k);
// Compact by default (lists): state + masked key tail + badge, full result on hover (title). full=true (detail
// page only): whole key + the full activation result inline.
function licCell(m,full){if(!m.license_tail)return '—';
  const ok=/activated successfully/i.test(m.license_result||'');
  const badge=ok?' '+ic('ok'):licErr(m)?' <span class="badge rounded-pill text-bg-danger">error</span>':'';
  if(!full)return `${esc(m.license_state||'')} …${esc(m.license_tail)}${badge}`;
  const res=m.license_result?`<div class="mono mt-1" style="white-space:pre-wrap">${esc(m.license_result)}</div>`:'';
  return `${esc(m.license_state||'')} ${esc(m.license_key||('…'+m.license_tail))}${badge}${res}`;}
async function setKey(id,who){
  let k='';
  do{k=await ask('Windows license key for '+who,k===''||keyOk(k)?'':KEY_RULE,k);if(k===null)return;k=k.trim().toUpperCase();}
  while(k&&!keyOk(k));
  try{await j('/api/machines/license',mk({id,key:k}));
    toast(k?'Key saved. It is installed when '+who+' boots next (its base is rebuilt once).':'Key removed.');refreshMachines();}
  catch(e){toast(e.message,true)}}
async function rearmKey(id,who){if(!await sure('Hand the key to '+who+' once more? Its base is rebuilt on the next boot.'))return;
  try{await j('/api/machines/license/rearm',mk({id}));refreshMachines();}catch(e){toast(e.message,true)}}

// ---- devices (devices.rs): full machine management; the Machines page stays the quick dashboard ----
let dvAll=[], dvImages=[], dvSel=new Set(), dvPending=null, dvOpenId=null;
const dvKey=m=>m.mac;   // selection key (new machines have no id yet)
const imgName=id=>(dvImages.find(i=>i.id===id)||{}).name||'';
async function loadDevices(){
  [dvAll,dvImages]=await Promise.all([j('/api/status'),j('/api/images')]);
  const bi=document.getElementById('dv_bulk_img');
  if(bi)bi.innerHTML='<option value="">(global default image)</option>'+dvImages.map(i=>`<option value="${i.id}">${esc(i.name)}</option>`).join('');
  const keys=new Set(dvAll.map(dvKey)); [...dvSel].forEach(k=>{if(!keys.has(k))dvSel.delete(k);});
  dvRender();
  if(dvPending){const id=dvPending;dvPending=null;dvOpen(id);} else if(dvOpenId)dvOpen(dvOpenId);
}
function dvFiltered(){
  const g=id=>((document.getElementById(id)||{}).value||'').trim().toLowerCase(), has=(val,q)=>!q||(val||'').toLowerCase().includes(q);
  const [host,grp,ip,mac,state,img,lic,notes]=['dvf_host','dvf_grp','dvf_ip','dvf_mac','dvf_state','dvf_img','dvf_lic','dvf_notes'].map(g);
  return dvAll.filter(m=>has(m.hostname,host)&&has(m.grp,grp)&&has(m.ip,ip)&&has(m.mac,mac)&&has(imgName(m.image_id),img)&&has(m.notes,notes)
    &&(!state||(state==='on'?m.online:state==='off'?!m.online:state==='registered'?m.registered:!m.registered))
    &&(!lic||(lic==='none'?!m.license_tail:lic==='error'?licErr(m):m.license_state===lic)));
}
function dvRender(){
  const rows=dvFiltered(), tb=document.querySelector('#devices tbody'); if(!tb)return;
  tb.innerHTML=rows.map(m=>`<tr class="${dvOpenId&&m.id===dvOpenId?'table-active':''}" ${m.registered?`${act('dvOpen',m.id)} style="cursor:pointer"`:''}>
      <td data-click="noop"><input class="form-check-input" type="checkbox" ${dvSel.has(dvKey(m))?'checked':''} data-change="dvToggle" data-args="${esc(JSON.stringify([dvKey(m)]))}"></td>
      <td>${m.hostname?esc(m.hostname):'<span class="mono">—</span>'} ${m.registered?'':'<span class="badge rounded-pill text-bg-warning">new</span>'}</td>
      <td class="mono">${esc(m.grp||'—')}</td><td class="mono">${esc(m.ip)}</td><td class="mono">${esc(m.mac)}</td>
      <td>${led(m.online)}</td>
      <td class="mono">${esc(imgName(m.image_id)||'—')}</td>
      <td class="mono" title="${esc(m.license_result||'')}">${licCell(m)}</td>
      <td style="white-space:normal">${esc(m.notes||'')}</td></tr>`).join('')
    ||'<tr><td colspan=9 class="mono">no matching machines</td></tr>';
  document.getElementById('dv_sel').textContent=dvSel.size+' selected';
  const all=document.getElementById('dv_all'); if(all)all.checked=rows.length>0&&rows.every(m=>dvSel.has(dvKey(m)));
}
function dvToggle(k,on){on?dvSel.add(k):dvSel.delete(k);document.getElementById('dv_sel').textContent=dvSel.size+' selected';}
function dvSelectAll(on){dvFiltered().forEach(m=>on?dvSel.add(dvKey(m)):dvSel.delete(dvKey(m)));dvRender();}
function dvMsg(t){say(document.getElementById('dv_msg'),t);}
const dvPicked=()=>dvAll.filter(m=>dvSel.has(dvKey(m)));
async function dvBulk(action){
  const reg=dvPicked().filter(m=>m.registered);
  if(!reg.length){toast('Select registered machines first',true);return;}
  let value='';
  if(action==='group'){value=v('dv_bulk_grp');if(value&&!/^[A-Za-z0-9_-]{1,32}$/.test(value)){toast('Group: letters/digits/_/-, max 32',true);return;}}
  if(action==='image')value=document.getElementById('dv_bulk_img').value;
  if(action==='delete'&&!await sure('Delete '+reg.length+' machine(s)? Their DHCP binding and license key go too.'))return;
  try{const r=await j('/api/machines/bulk',mk({ids:reg.map(m=>m.id),action,value}));dvMsg('✓ '+action+': '+r.count+' machine(s)');
    if(action==='delete'){dvSel.clear();dvClose();}loadDevices();}
  catch(e){dvMsg('✗ '+e.message);}
}
async function dvRegister(){
  const macs=dvPicked().filter(m=>!m.registered).map(m=>m.mac);
  if(!macs.length){toast('Select NEW machines (not registered yet)',true);return;}
  try{const r=await j('/api/machines/register-bulk',mk({macs,prefix:v('dv_prefix'),start:+v('dv_start')||1,digits:+document.getElementById('dv_digits').value}));
    dvMsg('✓ registered: '+r.registered.map(x=>x.hostname+' ('+x.mac+')').join(', ')+'\nReboot them to get their IP/name.');dvSel.clear();loadDevices();}
  catch(e){dvMsg('✗ '+e.message);}
}
async function dvImport(input){
  const f=input.files[0];input.value='';if(!f)return;
  try{const r=await fetch('/api/machines/import',{method:'POST',headers:{'content-type':'text/csv'},body:await f.text()});
    const t=await r.text();
    if(!r.ok){dvMsg('✗ import refused, nothing was written:\n'+t);return;}
    const o=JSON.parse(t);dvMsg('✓ import: '+o.added+' added, '+o.updated+' updated, '+o.keys+' license key(s) set');loadDevices();}
  catch(e){dvMsg('✗ '+e.message);}
}
async function dvAdd(){
  const mac=v('dv_add_mac'),ip=v('dv_add_ip')||null,hostname=v('dv_add_host')||null;
  if(!mac){toast('Enter a MAC',true);return;}
  if(hostname&&!hostOk(hostname)){dvMsg('✗ '+HOST_RULE);return;}
  try{await j('/api/machines',mk({mac,ip,hostname}));dvMsg('✓ added');['dv_add_mac','dv_add_ip','dv_add_host'].forEach(i=>document.getElementById(i).value='');loadDevices();}
  catch(e){dvMsg('✗ '+e.message);}
}
async function dvOpen(id){
  const box=document.getElementById('dv_detail'); if(!box)return;
  let d; try{d=await j('/api/machines/detail?id='+id);}catch(e){dvClose();return;}
  dvOpenId=id;
  const m=d.machine; m.license_key=d.license_key; // key is skip-serialized on machine; detail returns it separately
  const name=m.hostname||m.mac, who=esc(name), st=dvAll.find(x=>x.id===id)||{};
  const opt=dvImages.map(i=>`<option value="${i.id}" ${i.id===m.image_id?'selected':''}>${esc(i.name)}</option>`).join('');
  const lease=d.lease?esc(d.lease.ip||'—')+' · '+esc(d.lease.source)+' · '+(d.lease.expires_in>0?'expires in '+Math.round(d.lease.expires_in/60)+' min':'expired'):'—';
  box.hidden=false;
  const fld=(label,id,input)=>`<div><label class="form-label small mb-1" for="${id}">${label}</label>${input}</div>`;
  box.innerHTML=`<h2 class="h5 mb-3">${who} ${led(st.online)}</h2>
    <div class="d-flex flex-wrap gap-2 align-items-end">
      ${fld('Hostname','dd_host',`<input class="form-control form-control-sm" id="dd_host" value="${esc(m.hostname||'')}" size="12" maxlength="15">`)}
      ${fld('MAC','dd_mac',`<input class="form-control form-control-sm" id="dd_mac" value="${esc(m.mac)}" size="17">`)}
      ${fld('Fixed IP','dd_ip',`<input class="form-control form-control-sm" id="dd_ip" value="${esc(m.ip||'')}" size="12" placeholder="(none)">`)}
      ${fld('Group','dd_grp',`<input class="form-control form-control-sm" id="dd_grp" value="${esc(m.grp||'')}" size="8">`)}
      ${fld('Default image','dd_img',`<select class="form-select form-select-sm" id="dd_img"><option value="">(global default)</option>${opt}</select>`)}
      <div class="flex-grow-1" style="min-width:180px"><label class="form-label small mb-1" for="dd_notes">Notes</label><input class="form-control form-control-sm" id="dd_notes" value="${esc(m.notes||'')}" maxlength="200"></div>
    </div>
    <div class="d-flex flex-wrap gap-2 align-items-center mt-2">
      <button class="btn btn-sm btn-primary" ${act('dvSave',id)}>Save</button>
      <button class="btn btn-sm btn-outline-secondary" ${act('wake',m.mac)}>Wake</button>
      <button class="btn btn-sm btn-outline-secondary" ${act('setKey',id,name)}>License key</button>
      ${m.license_state==='sent'?`<button class="btn btn-sm btn-outline-secondary" ${act('rearmKey',id,name)}>Re-arm key</button>`:''}
      <button class="btn btn-sm btn-outline-danger" ${act('dvDelete',id,name)}>Delete</button>
      <button class="btn btn-sm btn-outline-secondary" ${act('dvClose')}>Close</button>
      <span id="dd_msg" class="msg"></span>
    </div>
    <table class="table table-sm align-middle mt-3 mb-0"><tbody>
      <tr><td class="mono">Lease</td><td class="mono">${lease}</td></tr>
      ${st.metrics?`<tr><td class="mono">Health</td><td class="mono" style="white-space:pre-line">${esc(mxText(st.metrics))}</td></tr>`:''}
      ${st.agent_since?`<tr><td class="mono">Up</td><td class="mono">${upFor(st.agent_since)} (agent connected ${new Date(st.agent_since*1000).toLocaleString()})</td></tr>`:''}
      ${st.traffic?`<tr><td class="mono">Traffic</td><td class="mono">${esc(trText(st.traffic))}</td></tr>`:''}
      <tr><td class="mono">License</td><td class="mono" style="white-space:normal">${licCell(m,true)}</td></tr>
      <tr><td class="mono">Remote key</td><td class="mono" style="white-space:normal">${!d.agent_key?'— none yet: its next Windows boot gets one'
        :d.agent_key.delivered?'handed over '+new Date(d.agent_key.delivered*1000).toLocaleString():'new key waiting for its next boot'}
        <button class="btn btn-sm btn-outline-secondary ms-2" ${act('rekey',m.mac,name)} data-owner title="a new key, handed to the machine at its next boot — when it lost its key (new disk) or the old one may have leaked">Re-issue</button></td></tr>
      <tr><td class="mono">Driver packages</td><td class="mono">${d.drivers.length?esc(d.drivers.join(', ')):'—'}</td></tr>
      <tr><td class="mono">Hardware (${d.hwids.length})</td><td class="mono" style="white-space:normal">${d.hwids.length
        ?`<details><summary>IDs its stage reported at the last boot</summary>${d.hwids.map(esc).join('<br>')}</details>`
        :'— (no Windows boot since this was added)'}</td></tr>
    </tbody></table>
    <h3 class="h6 mt-3">Disks</h3><div id="dd_disks" class="msg">…</div>`;
  dvRender();
  dvDisks(m.mac,name);
}
// The machine's local disks as its stage reported them (disks.rs): which one is the Broom disk, pick another one
// (erased at the next boot), hide the others from the guest.
async function dvDisks(mac,name){const el=document.getElementById('dd_disks');if(!el)return;
  let k;try{k=await j('/api/disks?mac='+encodeURIComponent(mac));}catch(e){el.textContent='✗ '+e.message;return;}
  const hide=k.hide==null?'':k.hide?'1':'0';
  const rows=k.disks.map(d=>{const label=`${d.dev} ${gbs(d.size)} ${d.model}`.trim();
    const role=d.broom?'<span class="badge text-bg-primary">Broom disk</span>'
      :k.broom===d.id?'<span class="badge text-bg-warning" title="erased and set up at the next boot">Broom disk at next boot</span>'
      :`<button class="btn btn-sm btn-outline-danger" ${act('dvBroomDisk',mac,d.id,label,name)} title="erase it at the next boot and keep Windows there instead">Use as Broom disk</button>`;
    return `<tr><td class="mono">${esc(d.dev)}</td><td class="mono">${gbs(d.size)}</td><td>${esc(d.model||'—')}</td><td class="mono small">${esc(d.id)}</td><td>${role}</td></tr>`;}).join('');
  el.innerHTML=`<div class="d-flex flex-wrap gap-2 align-items-center mb-2"><label class="small" for="dd_hide">Other disks</label>
      <select class="form-select form-select-sm w-auto" id="dd_hide" data-change="dvHide" data-args="${esc(JSON.stringify([mac]))}">
        <option value="" ${hide===''?'selected':''}>as its group (${k.group_hide?'hidden':'visible'})</option>
        <option value="1" ${hide==='1'?'selected':''}>hidden from the guest</option>
        <option value="0" ${hide==='0'?'selected':''}>visible</option></select>
      <span class="small text-body-secondary">hidden = offline in Windows, gone on a Linux image; applies from the next boot</span><span id="dk_msg" class="msg"></span></div>
    ${rows?`<table class="table table-sm align-middle mb-0"><thead><tr><th>Disk</th><th>Size</th><th>Model</th><th>Id</th><th></th></tr></thead><tbody>${rows}</tbody></table>
      <div class="small text-body-secondary mt-1">reported ${new Date(k.seen*1000).toLocaleString()} (Windows boots)</div>`
      :'<span class="mono">— not reported yet (boot Windows on it once)</span>'}`;}
async function rekey(mac,name){
  if(!await sure(`New Remote key for ${name}? Its agent is refused until its next boot brings the new key.`))return;
  try{await j('/api/vnc/rekey',mk({mac}));toast('New key: '+name+' gets it at its next boot');if(dvOpenId)dvOpen(dvOpenId);}catch(e){toast(e.message,true);}}
async function dvHide(mac,el){const m=document.getElementById('dk_msg');
  try{await j('/api/disks/hide',mk({mac,hide:el.value===''?null:el.value==='1'}));say(m,' ✓ saved');}catch(e){say(m,' ✗ '+e.message);}}
async function dvBroomDisk(mac,id,label,name){
  const t=await ask('Erase a disk',`At its next boot ${name} ERASES ${label} and keeps Windows there; its current Broom disk loses its Broom partitions (only Broom's own files are on it). Type the machine name to confirm.`,'');
  if(t==null)return;
  if(t.trim()!==name){toast('Name does not match — nothing changed',true);return;}
  try{await j('/api/disks/broom',mk({mac,id}));toast('Set: '+label+' becomes the Broom disk at the next boot');dvDisks(mac,name);}
  catch(e){toast(e.message,true);}}
function dvClose(){dvOpenId=null;const b=document.getElementById('dv_detail');if(b)b.hidden=true;dvRender();}
async function dvSave(id){
  const img=document.getElementById('dd_img').value, msg=document.getElementById('dd_msg');
  const body={id,mac:v('dd_mac'),ip:v('dd_ip')||null,hostname:v('dd_host')||null,grp:v('dd_grp')||null,notes:v('dd_notes')||null,image_id:img?+img:null};
  if(body.hostname&&!hostOk(body.hostname)){say(msg,' ✗ '+HOST_RULE);return;}
  try{await j('/api/machines/update',mk(body));say(msg,' ✓ saved');loadDevices();}
  catch(e){say(msg,' ✗ '+e.message);}
}
async function dvDelete(id,who){if(!await sure('Delete '+who+'? Its DHCP binding and license key go too.'))return;
  try{await j('/api/machines/delete',mk({ids:[id]}));dvClose();loadDevices();}catch(e){toast(e.message,true);}}

// ---- images ----
const gbs=b=>b>=1e9?(b/1e9).toFixed(1)+' GB':(b/1e6).toFixed(0)+' MB';
// Hardware health (metrics.rs, pushed by the agent). badge = compact line for the Machines table; bad disk / hot → red.
const mxDiskBad=d=>d.predict_fail||(d.realloc||0)>0||(d.pending||0)>0||(d.health&&!/^(ok|healthy)$/i.test(d.health));
function mxBadge(m){if(!m)return'';
  const hot=m.temps&&m.temps.some(t=>t>=85), badDisk=(m.disks||[]).some(mxDiskBad);
  const bits=[`CPU ${m.cpu}%`,`RAM ${m.ram}%`];
  if(m.temps&&m.temps.length)bits.push(Math.max(...m.temps)+'°C');
  bits.push('⛁ '+(badDisk?'check':'OK'));
  return ` <span class="badge ${hot||badDisk?'text-bg-danger':'text-bg-light border'}" title="${esc(mxText(m))}">${bits.join(' · ')}</span>`;}
// Uptime ≈ since the agent connected (it starts with Windows); traffic = what the server sends it now (sysmon.rs).
function dur(secs){const s=Math.max(0,secs|0),d=s/86400|0,h=(s%86400)/3600|0,m=(s%3600)/60|0;
  return d?`${d}d ${h}h`:h?`${h}h ${m}m`:`${m}m`;}
const upFor=since=>dur(Date.now()/1000-since);
const trText=t=>`iSCSI ${(t[0]/1e6).toFixed(1)} MB/s · golden HTTP ${(t[1]/1e6).toFixed(1)} MB/s`;
function upBadge(m){let s='';
  if(m.agent_since)s+=` <span class="badge text-bg-light border" title="agent connected ${new Date(m.agent_since*1000).toLocaleString()}">up ${upFor(m.agent_since)}</span>`;
  const t=m.traffic;if(t&&t[0]+t[1]>=1e5)s+=` <span class="badge text-bg-info" title="${esc(trText(t))}">↓ ${((t[0]+t[1])/1e6).toFixed(1)} MB/s</span>`;
  if(m.dns_blocked&&m.online)s+=` <span class="badge text-bg-warning" title="the DNS server refuses its names (no Remote agent, no recent PXE boot, not exempt): no internet — Network page → DNS">no DNS</span>`;
  return s;}
function mxText(m){const l=[`CPU ${m.cpu}%  RAM ${m.ram}%`];
  if(m.temps&&m.temps.length)l.push('ACPI temps (best-effort): '+m.temps.map(t=>t+'°C').join(', '));
  for(const d of (m.disks||[])){let s=d.name+': '+(d.health||'?');
    if(d.temp!=null)s+=', '+d.temp+'°C';
    if(d.realloc!=null||d.pending!=null)s+=', bad sectors '+(d.realloc||0)+' reallocated / '+(d.pending||0)+' pending';
    if(d.predict_fail)s+=' — SMART predicts FAILURE';
    l.push(s);}
  if(m.free!=null)l.push('system drive free: '+gbs(m.free));
  return l.join('\n');}
let imgRows=[];
async function loadImages(){
  const [rows]=await Promise.all([j("/api/images"),loadVols()]);imgRows=rows;
  document.querySelector('#images tbody').innerHTML=rows.map(i=>
    `<tr><td>${esc(i.name)}${i.export?`<div class="mono" style="font-size:12px" title="exported ${i.export.created?new Date(i.export.created*1000).toLocaleString():''} — save both into one folder, open the .vmx in VMware">${ic('down','')} <a href="/api/images/export-file?id=${i.id}&f=vmx">.vmx</a> · <a href="/api/images/export-file?id=${i.id}&f=vmdk">.vmdk ${gbs(i.export.size)}</a> (${esc(i.export.version)})</div>`:''}</td><td>${osPill(i.os)}</td>
       <td><a href="#" ${act('editImgGroups',i.id,i.name,i.groups)} title="machine groups that get it in their boot menu">${i.groups.length?esc(i.groups.join(', ')):'all'}</a></td>
       <td class="mono">${i.cache_mode==='zram'?'RAM':'disk'}</td>
       <td>${i.is_default?ic('ok'):''}</td>
       <td>${i.boot_script?ic('ok'):'<span class="mono">no</span>'}</td>
       <td class="mono" title="used on disk / virtual disk">${i.size==null?'—':gbs(i.used)+' / '+gbs(i.size)}${i.active_version?' <span class="badge rounded-pill text-bg-success">'+esc(i.active_version)+'</span>':''}</td>
       <td>${volCell('image',i.name,i.volume)}</td>
       <td class="mono" title="${esc(i.hash||'')}">${i.hash?esc(i.hash).slice(0,10):'—'}</td>
       <td><div class="d-flex flex-wrap gap-1" style="min-width:320px">
         <button class="btn btn-sm btn-outline-secondary" ${act('setDefault',i.id)}>Default</button>
         <button class="btn btn-sm btn-outline-secondary" ${act('toggleCache',i.id,i.cache_mode||'disk',i.name)}>${(i.cache_mode==='zram')?'→disk':'→RAM'}</button>
         ${i.os==='windows'?`<button class="btn btn-sm btn-outline-secondary" ${act('toggleBase',i.id,!i.base_mode)} title="BASE MODE: the first logon on each machine waits for a technician to set up apps, then restart (saved for every boot). Off: base is saved by itself.">${i.base_mode?'Base mode: ON':'Base mode: off'}</button>`:`<button class="btn btn-sm btn-outline-secondary" ${act('toggleSsd',i.id,!i.use_ssd)} title="On: the golden is cached and the session's writes go to the machine's SSD (reset every boot). Off (one-time): nothing touches the SSD — golden over the network, writes in RAM, gone at power-off.">${i.use_ssd?'SSD: on':'SSD: off (one-time)'}</button>`}
         ${i.os==='windows'?`<button class="btn btn-sm btn-outline-secondary" ${act('togglePreload',i.id,!i.preload)} title="On: every machine that may boot it keeps it on its SSD ahead of use (fetched while another image boots: that boot takes longer once), so choosing it needs no download (its base is still built on its first boot). Off: fetched the first time it is chosen.">${i.preload?'Preload: on':'Preload: off'}</button>`:''}
         <button class="btn btn-sm btn-outline-secondary" ${act('republish',i.id,i.name)}>Republish</button>
         <button class="btn btn-sm btn-outline-secondary" ${act('showVersions',i.id,i.name,i.os)}>Versions</button>
         <button class="btn btn-sm btn-outline-secondary" ${act('exportImage',i.id,i.name,null)} title="download as a VMware VM (.vmx + .vmdk) to edit the golden">Export</button>
         <button class="btn btn-sm btn-outline-secondary" ${act('editBoot',i.id)}>Boot</button>
         <button class="btn btn-sm btn-outline-danger" ${act('delImage',i.id,i.name)}>Delete</button>
       </div></td></tr>`).join('') || '<tr><td colspan=10 class="mono">no images yet</td></tr>';
  // A job still running that this tab doesn't follow (page reloaded / opened meanwhile) → follow it on the status line.
  for(const i of rows)if(i.job&&i.job.startsWith('⏳')&&!jobWatch[i.name])watchJob(i.name,document.getElementById('img_status'));
  // srvhost inside the images fragment → set after it is injected.
  for(const id of ['srvhost2','srvhost3']){const e=document.getElementById(id);if(e)e.textContent=location.host;}
}
async function toggleCache(id,cur,name){const mode=cur==='zram'?'disk':'zram';
  if(!await sure('Switch image cache to "'+(mode==='zram'?'RAM':'disk')+'"? (republish; RAM keeps a compressed copy of the golden in memory and serves clients from it)'))return;
  const el=document.getElementById('img_status');
  try{await j('/api/images/cache-mode',mk({id,mode}));watchJob(name,el);}catch(e){say(el,' ✗ '+e.message);}}
async function toggleBase(id,on){
  if(on&&!await sure('BASE MODE: every machine that builds its base (first boot, new golden, rename, drivers) will wait on the desktop until someone restarts it — whatever is done before that restart is kept for good. Turn on?'))return;
  const el=document.getElementById('img_status');
  try{await j('/api/images/base-mode',mk({id,on}));loadImages();}catch(e){say(el,' ✗ '+e.message);}}
async function toggleSsd(id,on){
  const el=document.getElementById('img_status');
  try{await j('/api/images/ssd',mk({id,on}));loadImages();}catch(e){say(el,' ✗ '+e.message);}}
async function togglePreload(id,on){
  const el=document.getElementById('img_status');
  try{await j('/api/images/preload',mk({id,on}));loadImages();}catch(e){say(el,' ✗ '+e.message);}}
async function editImgGroups(id,name,groups,el,e){e.preventDefault();
  const g=await ask('Groups of '+name,'Machine groups that get '+name+' in their boot menu, comma-separated (Devices page groups). Empty = every machine.',groups.join(', '));
  if(g===null)return;const st=document.getElementById('img_status');
  try{await j('/api/images/groups',mk({id,groups:[g]}));loadImages();}catch(err){say(st,' ✗ '+err.message);}}
async function setDefault(id){await j('/api/images/default',mk({id}));loadImages()}
// Job status by image name, pushed over SSE (/api/events "job") → text of el until ✓/✗.
// done(): optional, called when the job finishes (e.g. refresh the versions list).
const jobWatch={};
function showJob(name,s){const w=jobWatch[name];if(!w||!s)return;
  // The page may have been rebuilt since the watch started (tab switch): then the Images page's status line.
  const el=w.el.isConnected?w.el:document.getElementById(name.startsWith('bench:')?'bench_msg':'img_status');
  say(el,' '+(el===w.el?'':name+': ')+s);
  if(s.startsWith('✓')||s.startsWith('✗')){delete jobWatch[name];
    if(curPage==='images')loadImages().then(()=>{   // an open Versions panel of this image follows too (new v1, rollback…)
      const r=imgRows.find(x=>x.name===name);if(r&&verImg&&verImg.name===name)showVersions(r.id,r.name,r.os);}).catch(()=>{});
    if(w.done)w.done();}}
function watchJob(name,el,done){
  say(el,' ⏳ working...');jobWatch[name]={el,done};
  // catch up once: the job may have moved on (or finished) before this tab started watching
  j('/api/images/job?name='+encodeURIComponent(name)).then(r=>showJob(name,r.status)).catch(()=>{});
}
// ---- image versions (versions.rs: dedup 4 MB chunks; snapshot/rollback run as jobs) ----
let verImg=null;
async function showVersions(id,name,os){verImg={id,name,os};
  const el=document.getElementById('img_versions');
  el.innerHTML=`<h2 class="h6 mb-2">Versions — ${esc(name)}</h2><p class="mono">${ic('wait')} loading...</p>`;
  const r=await j('/api/images/snapshots?id='+id);
  el.innerHTML=`<h2 class="h6 mb-2">Versions — ${esc(name)} <button class="btn btn-sm btn-outline-primary ms-2" ${act('snapshotImage')}>+ Snapshot now</button></h2>
    <div class="table-responsive"><table class="table table-sm table-hover align-middle"><thead><tr><th>Version</th><th>Label</th><th>Created</th><th></th></tr></thead><tbody>${
    r.versions.map(v=>`<tr><td class="mono">${esc(v.version)} ${v.version===r.active?'<span class="badge rounded-pill text-bg-success">active</span>':''}</td>
      <td>${esc(v.label)}</td><td class="mono">${new Date(v.created*1000).toLocaleString()}</td>
      <td><div class="d-flex flex-wrap gap-1" style="min-width:320px"><button class="btn btn-sm btn-outline-secondary" ${act('rollbackImage',v.version)}>Rollback</button>
        <button class="btn btn-sm btn-outline-secondary" ${act('exportImage',id,name,v.version)}>Export</button>
        <button class="btn btn-sm btn-outline-secondary" ${act('versionToImage',v.version)} title="add this version as a new image on the list">→ New image</button>
        <button class="btn btn-sm btn-outline-danger" ${act('deleteVersion',v.version)}>Delete</button></div></td></tr>`).join('')
    || '<tr><td colspan=5 class="mono">no versions yet — Snapshot now saves the current golden</td></tr>'}</tbody></table></div>`;}
async function snapshotImage(){const label=await ask('Snapshot '+verImg.name,'Label for this version (optional):','');if(label===null)return;
  const el=document.getElementById('img_status');
  try{await j('/api/images/snapshot',mk({id:verImg.id,label}));watchJob(verImg.name,el,()=>showVersions(verImg.id,verImg.name,verImg.os));}catch(e){say(el,' ✗ '+e.message);}}
async function rollbackImage(version){
  if(!await sure('Roll '+verImg.name+' back to '+version+'?'+(verImg.os==='windows'?' Every client will download the golden again.':' Running clients lose their iSCSI disk until publish finishes.')))return;
  const el=document.getElementById('img_status');
  try{await j('/api/images/rollback',mk({id:verImg.id,version}));watchJob(verImg.name,el,()=>showVersions(verImg.id,verImg.name,verImg.os));}catch(e){say(el,' ✗ '+e.message);}}
async function deleteVersion(version){if(!await sure('Delete version '+version+' of '+verImg.name+'?'))return;
  try{await j('/api/images/version-delete',mk({id:verImg.id,version}));}catch(e){toast(e.message,true);}
  showVersions(verImg.id,verImg.name,verImg.os);}
async function exportImage(id,name,version){
  if(!await sure('Export '+name+(version?' '+version:' (current golden)')+' as a VMware VM (.vmx + .vmdk)?\nNeeds free disk space on the server ≈ the golden size; replaces the previous export of this image.'))return;
  const el=document.getElementById('img_status');
  try{await j('/api/images/export',mk({id,version}));watchJob(name,el);}catch(e){say(el,' ✗ '+e.message);}}
async function versionToImage(version){
  const name=await ask('Version '+version+' → new image','New image name for '+verImg.name+' '+version+' (letters, digits, _ -):',verImg.name+'-'+version);
  if(!name)return;
  const el=document.getElementById('img_status');
  try{await j('/api/images/from-version',mk({id:verImg.id,version,name}));loadImages();watchJob(name,el);}catch(e){say(el,' ✗ '+e.message);}}
async function republish(id,name){const el=document.getElementById('img_status');
  try{await j('/api/images/publish',mk({id}));watchJob(name,el);}catch(e){say(el,' ✗ '+e.message);}}
async function editBoot(id){const cur=(imgRows.find(i=>i.id===id)||{}).boot_script||'';const s=await ask('iPXE boot script','',cur,true);if(s===null)return;
  await j('/api/images/boot-script',mk({id,boot_script:s}));loadImages();}
async function delImage(id,name){if(!await sure('Delete image "'+name+'"?'))return;
  await j('/api/images/delete',mk({id}));loadImages();}
// Chunked upload: every file in 8 MB pieces, 4 in flight; a failed piece is retried (2,4,8,16,32 s).
const CHUNK=8<<20,LANES=4;
async function putChunk(api,name,f,off){
  const url=api+'/upload-chunk?name='+encodeURIComponent(name)+'&file='+encodeURIComponent(f.name)+'&offset='+off+'&total='+f.size;
  for(let t=0;;t++){
    let r=null;try{r=await fetch(url,{method:'PUT',body:f.slice(off,off+CHUNK)});}catch(_){}   // null = network error
    if(r&&r.ok)return;
    if(r&&r.status<500)throw new Error(await r.text());   // rejected: retrying won't help
    if(t>=5)throw new Error(r?await r.text():'network error');
    await new Promise(ok=>setTimeout(ok,2000<<t));
  }
}
// api = '/api/images' (golden) or '/api/drivers' (driver package): same start / chunk / done protocol.
async function uploadFiles(api,name,files,prog,msg){
  await j(api+'/upload-start',mk({name}));
  const q=[];for(const f of files)for(let o=0;o===0||o<f.size;o+=CHUNK)q.push([f,o]);
  const total=files.reduce((s,f)=>s+f.size,0)||1,t0=Date.now();let done=0;
  const bar=prog.firstElementChild;prog.style.display='';bar.style.width='0%';
  const lane=async()=>{while(q.length){const [f,o]=q.shift();
    try{await putChunk(api,name,f,o);}catch(e){q.length=0;throw e;}   // one piece failed for good → stop all lanes
    done+=Math.min(CHUNK,f.size-o);bar.style.width=done/total*100+'%';
    msg.textContent=' upload '+Math.round(done/total*100)+'% · '+(done/1e6/Math.max(1,(Date.now()-t0)/1000)).toFixed(0)+' MB/s';}};
  await Promise.all(Array.from({length:LANES},lane));
  msg.textContent=' upload 100% — finishing on the server…';   // driver packages: unzip + read .inf + pack
  return j(api+'/upload-done',mk({name}));
}
async function addImage(){
  const dir=[...document.getElementById('up_dir').files].filter(f=>/\.(vmdk|vmx|img|raw)$/i.test(f.name));
  const one=document.getElementById('up_file').files[0],files=dir.length?dir:(one?[one]:[]);
  const name=v('up_name'),msg=document.getElementById('up_msg'),cache=document.getElementById('up_cache').value;
  if(!files.length||!name){toast('Enter a name + choose a VM folder or a file',true);return;}
  const os=document.getElementById('up_os').value;
  try{await j('/api/images',mk({name,os,cache_mode:cache,volume:v('up_vol')||null}));}catch(e){}
  try{await uploadFiles('/api/images',name,files,document.getElementById('up_prog'),msg);}
  catch(e){say(msg,' ✗ '+e.message);return;}
  say(msg,' ✓ upload done');loadImages();watchJob(name,msg);   // convert + publish run in the background → SSE
}

// ---- server disks (volumes.rs): more disks for images and games disks ----
let vols={volumes:[],disks:[],home:null};
const volFree=s=>s?gbs(Math.max(0,s.total-s.used))+' free':'?';
// The broom disks (Images / Games pages): the pickers for a new image / games disk, shown once there is a second one.
async function loadVols(){try{vols=await j('/api/volumes');}catch(e){return;}
  const up=vols.volumes.filter(x=>x.mounted);
  const opts=`<option value="">broom folder (${volFree(vols.home&&vols.home.space)})</option>`
    +up.map(x=>`<option value="${esc(x.id)}">${esc(x.id)} (${volFree(x.space)})</option>`).join('');
  for(const id of ['up_vol','gm_vol']){const s=document.getElementById(id);if(s)s.innerHTML=opts;}
  document.querySelectorAll('.vol-pick').forEach(e=>e.hidden=!up.length);}
// The disk an image / games disk is on; another one picked = moved there (a job).
function volCell(kind,name,cur){
  if(!vols.volumes.length&&!cur)return '<span class="mono">broom folder</span>';
  const o=(id,label,dis)=>`<option value="${esc(id)}"${(cur||'')===id?' selected':''}${dis?' disabled':''}>${esc(label)}</option>`;
  return `<select class="form-select form-select-sm w-auto" data-owner data-change="volMove" data-args="${esc(JSON.stringify([kind,name,cur||'']))}">`
    +o('','broom folder')+vols.volumes.map(x=>o(x.id,x.id+(x.mounted?'':' (missing)'),!x.mounted&&x.id!==cur)).join('')+'</select>';}
async function volMove(kind,name,cur,el){const to=el.value,what=(kind==='image'?'image ':'iSCSI disk ')+name;
  if(!await sure(`Move ${what} to ${to||'the broom folder'}? Its files are copied (a while for a big one), then the old copy is deleted. Machines using it must be shut down first.`)){el.value=cur;return;}
  const job=kind==='image'?name:'games:'+name,m=document.getElementById(kind==='image'?'img_status':'gm_msg');
  try{await j('/api/volumes/move',mk({kind,name,to}));watchJob(job,m,()=>kind==='image'?loadImages():loadGames());}
  catch(e){toast(e.message,true);el.value=cur;}}
// Server page → Disks: every disk of the server, broom's disks, format a free one / add a mounted folder.
async function loadServerDisks(){const el=document.getElementById('b_disks');if(!el)return;
  let r;try{r=await j('/api/volumes?disks=1');}catch(e){el.textContent='✗ '+e.message;return;}
  vols=r;
  const sp=s=>s?`${gbs(s.used)} used of ${gbs(s.total)}`:'not mounted';
  const on=x=>[...x.images.map(n=>'image '+n),...x.games.map(n=>'iSCSI disk '+n)].join(', ');
  const vrows=`<tr><td><b>broom folder</b></td><td class="mono">${esc(r.home.path)}</td><td class="mono">${sp(r.home.space)}</td><td>everything not on another disk</td><td></td></tr>`
    +r.volumes.map(x=>`<tr><td><b>${esc(x.id)}</b>${x.mounted?'':' <span class="badge text-bg-danger" title="not mounted: unplugged? what is on it is not served">missing</span>'}</td>
      <td class="mono">${esc(x.path)}${x.fstype?' ('+esc(x.fstype)+')':''}</td><td class="mono">${sp(x.space)}</td><td>${esc(on(x))||'—'}</td>
      <td data-owner>${on(x)?'':`<button class="btn btn-sm btn-outline-secondary" ${act('volRemove',x.id)} title="stop using it (its files stay)">Remove</button>`}</td></tr>`).join('');
  const badge={system:'text-bg-secondary','in use':'text-bg-secondary',broom:'text-bg-primary',free:'text-bg-success'};
  const drows=r.disks.map(d=>`<tr><td class="mono">${esc(d.name)}</td><td class="mono">${gbs(d.size)}</td><td>${esc(d.model||'—')}${d.rotational?' <span class="small text-body-secondary">HDD</span>':''}</td>
      <td class="mono small">${esc([d.mount||'',...d.parts.map(p=>p.name+(p.mount?' → '+p.mount:'')),...d.holders.map(h=>'held by '+h)].filter(Boolean).join(', '))||'—'}</td>
      <td class="small">${d.health?`<span class="${srvDiskBad(d.health)?'text-danger':''}">${esc(srvDiskHealth(d.health))}</span>`:'—'}</td>
      <td><span class="badge ${badge[d.state]}">${d.state}</span></td>
      <td data-owner>${d.state==='free'?`<button class="btn btn-sm btn-outline-danger" ${act('volFormat',d.name,d.model,d.size,d.old_data)}>Format</button>`:''}</td></tr>`).join('');
  el.innerHTML=`<table class="table table-sm align-middle mb-3"><thead><tr><th>Broom disk</th><th>Path</th><th>Space</th><th>On it</th><th></th></tr></thead><tbody>${vrows}</tbody></table>
    <table class="table table-sm align-middle mb-2"><thead><tr><th>Disk</th><th>Size</th><th>Model</th><th>Partitions / mounts</th><th>Health</th><th>State</th><th></th></tr></thead><tbody>${drows}</tbody></table>
    <div class="d-flex flex-wrap gap-2 align-items-end" data-owner><div><label class="form-label small mb-1" for="vol_folder">Or a folder you mounted yourself (LVM, RAID…)</label><input class="form-control form-control-sm" id="vol_folder" size="28" placeholder="/mnt/data/broom"></div>
      <button class="btn btn-sm btn-outline-primary" data-click="volFolder">Add folder</button><span id="vol_msg" class="msg align-self-center"></span></div>`;}
async function volFormat(name,model,size,old){
  const t=await ask('Format '+name,`ERASES everything on ${name} (${model||'disk'} ${gbs(size)})${old?' — it has partitions / a filesystem now':''} and makes it a broom disk for images and iSCSI disks. Type ${name} to confirm.`,'');
  if(t==null)return;
  if(t.trim()!==name){toast('Name does not match — nothing changed',true);return;}
  const m=document.getElementById('vol_msg');say(m,' formatting '+name+'…');
  try{const r=await j('/api/volumes/format',mk({disk:name,confirm:t.trim()}));say(m,' ✓ '+name+' is broom disk '+r.id);}catch(e){say(m,' ✗ '+e.message);}
  loadServerDisks();}
async function volFolder(){const m=document.getElementById('vol_msg');
  try{await j('/api/volumes/folder',mk({path:v('vol_folder')}));say(m,' ✓ added');}catch(e){say(m,' ✗ '+e.message);}loadServerDisks();}
async function volRemove(id){if(!await sure('Stop using broom disk '+id+'? It is unmounted if broom mounted it; its files stay on it.'))return;
  try{await j('/api/volumes/remove',mk({id}));}catch(e){toast(e.message,true);}loadServerDisks();}

// ---- driver packages (drivers.rs): .zip of an extracted driver folder → installed into base by broom-done ----
async function loadDrivers(){
  const rows=await j('/api/drivers');
  document.querySelector('#drivers tbody').innerHTML=rows.map(d=>
    `<tr><td>${esc(d.name)}</td><td class="mono">${gbs(d.size)}</td>
       <td class="mono" title="${esc(d.hwids.slice(0,40).join('\n'))}${d.hwids.length>40?'\n…':''}">${d.hwids.length}</td>
       <td class="mono" style="white-space:normal">${d.machines.length?esc(d.machines.join(', ')):'—'}</td>
       <td><input class="form-check-input" type="checkbox" id="drv_all_${d.id}" ${d.all_machines?'checked':''}></td>
       <td><input class="form-control form-control-sm" id="drv_grp_${d.id}" value="${esc(d.groups.join(', '))}" placeholder="VIP, Pro" size="12"></td>
       <td><div class="d-flex gap-1"><button class="btn btn-sm btn-outline-secondary" ${act('saveDrvTargets',d.id)}>Save</button>
         <button class="btn btn-sm btn-outline-danger" ${act('delDriver',d.id,d.name)}>Delete</button></div></td></tr>`).join('')
    || '<tr><td colspan=7 class="mono">no driver packages yet</td></tr>';
}
async function uploadDriver(){
  const f=document.getElementById('drv_file').files[0],msg=document.getElementById('drv_msg');
  const name=v('drv_name')||(f?f.name.replace(/\.zip$/i,'').replace(/[^A-Za-z0-9_-]/g,'-'):'');
  if(!f){toast('Choose the .zip of an extracted driver folder',true);return;}
  try{const r=await uploadFiles('/api/drivers',name,[f],document.getElementById('drv_prog'),msg);
    say(msg,' ✓ '+name+': '+r.infs+' .inf, '+r.hwids+' hardware IDs');loadDrivers();}
  catch(e){say(msg,' ✗ '+e.message);}
}
async function saveDrvTargets(id){
  const all_machines=document.getElementById('drv_all_'+id).checked, groups=document.getElementById('drv_grp_'+id).value;
  try{await j('/api/drivers/targets',mk({id,all_machines,groups}));loadDrivers();}catch(e){toast(e.message,true)}}
async function delDriver(id,name){if(!await sure('Delete driver package "'+name+'"? Machines that had it rebuild their base on the next boot.'))return;
  try{await j('/api/drivers/delete',mk({id}));loadDrivers();}catch(e){toast(e.message,true)}}

// ---- network ----
// DNS servers: one box each (order = preference), up to 8; sent to the server as one comma-separated list.
const DNS_MAX=8;
function dnsBox(v){const w=document.createElement('div');w.className='input-group input-group-sm w-auto';
  w.innerHTML='<input class="dns form-control" size="14" placeholder="e.g. 1.1.1.1"><button type="button" class="btn btn-outline-secondary" title="remove" aria-label="remove">×</button>';
  w.querySelector('input').value=v||'';w.querySelector('button').onclick=()=>{w.remove();dnsButton();};return w;}
function dnsButton(){const l=document.getElementById('dns_list');if(!l)return;let b=document.getElementById('dns_add');
  if(!b){b=document.createElement('button');b.id='dns_add';b.type='button';b.className='btn btn-sm btn-outline-secondary';b.textContent='+ DNS';
    b.onclick=()=>{l.insertBefore(dnsBox(''),b);dnsButton();};l.appendChild(b);}
  b.style.display=l.querySelectorAll('input.dns').length>=DNS_MAX?'none':'';}
function setDns(csv){const l=document.getElementById('dns_list');if(!l)return;l.innerHTML='';
  const v=(csv||'').split(',').map(s=>s.trim()).filter(Boolean);for(const x of (v.length?v:[''])) l.appendChild(dnsBox(x));dnsButton();}
function getDns(){return [...document.querySelectorAll('#dns_list input.dns')].map(e=>e.value.trim()).filter(Boolean).join(',');}
// DNS for healthy machines only (dns.rs).
async function loadDnsGate(){const box=document.getElementById('dns_groups');if(!box)return;
  let d,g;try{[d,g]=await Promise.all([j('/api/dns'),j('/api/groups')]);}catch(e){return;}
  document.getElementById('dns_mode').value=d.mode;document.getElementById('dns_grace').value=d.grace_min;
  document.getElementById('dns_resolve').value=d.resolve;document.getElementById('dns_up').value=d.upstream.join(', ');
  document.getElementById('dns_ips').value=d.exempt_ips;dnsResolveShow();
  // DNS on: the DHCP card hands out this server — its own DNS list is not used then.
  const on=d.mode==='on';document.getElementById('dhcp_dns_field').hidden=on;document.getElementById('dhcp_dns_self').hidden=!on;
  box.innerHTML=g.groups.map(n=>`<label class="form-check"><input class="form-check-input" type="checkbox" value="${esc(n)}"${d.exempt_groups.some(x=>x.toLowerCase()===n.toLowerCase())?' checked':''}> ${esc(n)}</label>`).join('')||'<span class="mono">no groups yet</span>';
  const ups=d.upstream.join(', '),how=d.resolve==='recursive'?`recursive: cache (${d.cached} answers), then ${ups||'the root servers'}`:`passthrough to ${ups}`;
  document.getElementById('dnsg_state').textContent=d.active
    ?`Running on ${d.server}:53, ${how} · ${d.allowed} IPs may resolve now`+(d.dhcp_full?'':` · the DHCP server is off: set the router's DHCP DNS to ${d.server}`)
    :'Off: clients use the DNS servers of the DHCP card.';}
function dnsResolveShow(){const l=document.getElementById('dns_up_label');if(l)l.textContent=v('dns_resolve')==='recursive'?'Asked on a cache miss (empty = root servers)':'DNS servers';}
async function saveDnsGate(){const m=document.getElementById('dnsg_msg');
  const body={mode:v('dns_mode'),resolve:v('dns_resolve'),upstream:v('dns_up').split(/[\s,]+/).filter(Boolean),
    grace_min:+v('dns_grace'),exempt_ips:document.getElementById('dns_ips').value,
    exempt_groups:[...document.querySelectorAll('#dns_groups input:checked')].map(e=>e.value)};
  try{const r=await j('/api/dns',mk(body));say(m,' ✓ '+(r.status||'saved'));}catch(e){say(m,' ✗ '+e.message);}loadDnsGate();}
async function loadDhcp(){loadDnsGate();const c=await j('/api/dhcp');
  ['mode','iface','server_ip','subnet','netmask','range_start','range_end','gateway'].forEach(k=>{const e=document.getElementById('dhcp_'+k);if(e)e.value=c[k]||'';});
  for(const k of ['ipxe_signed','strict_reset','rapid_commit','ipxe_fast','authoritative','send_hostname']){const e=document.getElementById('dhcp_'+k);if(e)e.checked=!!c[k];}
  setDns(c.dns);
  // Pool vs the machines that need an address from it (registered without a fixed IP + other devices with a lease).
  const pl=document.getElementById('dhcp_pool');
  if(pl){const tight=c.pool_need>c.pool_size*0.9;
    pl.className='small mt-2'+(tight?' alert alert-warning py-1 px-2 mb-0':' text-body-secondary');
    pl.textContent=`Pool: ${c.pool_size} addresses, ${c.pool_need} machines/devices need one from it.`
      +(tight?' Too tight: widen the range, or give machines a fixed IP (Devices).':'');}
  const dm=document.getElementById('dhcp_mode');if(dm)dm.onchange=toggleFull;toggleFull();}
function toggleFull(){const dm=document.getElementById('dhcp_mode');if(!dm)return;
  for(const id of ['full_only','full_only_opts']){const e=document.getElementById(id);if(e)e.style.display=dm.value==='full'?'':'none';}}
async function applyDhcp(){const b={};['mode','iface','server_ip','subnet','netmask','range_start','range_end','gateway'].forEach(k=>b[k]=document.getElementById('dhcp_'+k).value);
  for(const k of ['ipxe_signed','strict_reset','rapid_commit','ipxe_fast','authoritative','send_hostname']){const e=document.getElementById('dhcp_'+k);if(e)b[k]=e.checked?'1':'0';}
  b.dns=getDns();
  const m=document.getElementById('dhcp_msg');
  try{const r=await j('/api/dhcp',mk(b));say(m,' ✓ '+(r.status||'applied'));}catch(e){say(m,' ✗ '+e.message);}}

// ---- games disks (Games page) ----
let gmDisks=[];
async function loadGames(){const [g]=await Promise.all([j('/api/games'),loadVols()]);const tb=document.querySelector('#games tbody');if(!tb)return;
  gmDisks=g.disks;
  document.getElementById('gm_grouplist').innerHTML=g.groups.map(x=>`<option value="${esc(x)}">`).join('');
  const opts=sel=>'<option value="">— none —</option>'+g.machines.map(m=>
    `<option value="${m.id}"${m.id===sel?' selected':''}>${esc(m.name)}${m.grp?' ('+esc(m.grp)+')':''}</option>`).join('');
  tb.innerHTML=g.disks.map(x=>{const d=x.disk,s=x.status;
    return `<tr><td>${esc(d.name)}</td><td class="mono">${esc(d.letter)}:</td><td class="mono">${d.size_gb} GB</td>
      <td>${d.groups.length?esc(d.groups.join(', ')):'<span class="mono">all</span>'}</td>
      <td class="mono">${s?'g'+s.ver+(s.layers?` <span title="versions not merged into the disk yet (${gbs(s.layer_bytes)})">+${s.layers} ${s.merging?'merging…':'to merge'}</span>`:''):'<span class="text-danger">not served</span>'}</td>
      <td class="mono">${s?s.sessions+(s.old_sessions?` <span title="still on an older version until they reboot">(${s.old_sessions} older)</span>`:''):''}</td>
      <td><select class="form-select form-select-sm d-inline-block w-auto" data-owner data-change="setGamesUpdate" data-args="${esc(JSON.stringify([d.name]))}">${opts(d.update)}</select>${d.update!=null&&!x.update_ip?' <span class="text-danger" title="no fixed IP or lease known">no IP</span>':''}${s&&s.update_connected?' <span class="text-success">'+ic('dot','')+' writable</span>':''}</td>
      <td class="mono">${s&&s.update_bytes?gbs(s.update_bytes)+` <button class="btn btn-sm btn-outline-primary" ${act('saveGamesUpdate',d.name)}>Save</button> <button class="btn btn-sm btn-outline-secondary" ${act('discardGamesUpdate',d.name)}>Discard</button>`:'—'}</td>
      <td>${volCell('games',d.name,x.volume)}</td>
      <td data-owner><button class="btn btn-sm btn-outline-secondary" ${act('toggleGamesRam',d.name,!d.ram)} title="RAM: the disk is served from a copy in the server's RAM (loaded in the background; needs about as much free RAM as the data on it — games barely compress). Disk: read from its file (the page cache keeps what is read most).">${d.ram?(s&&s.ram?'Cache: RAM':(s&&s.ram_loading?'Cache: RAM (loading…)':'Cache: RAM (not loaded)')):'Cache: disk'}</button> <button class="btn btn-sm btn-outline-secondary" ${act('editGames',d.name)}>Edit</button> <button class="btn btn-sm btn-outline-danger" ${act('delGames',d.name)}>Delete</button></td></tr>`;}).join('')
    ||'<tr><td colspan="10" class="mono">no iSCSI disk yet</td></tr>';
  const m=document.getElementById('gm_msg');if(m&&g.error)say(m,' ✗ not served: '+g.error);}
async function gamesCall(url,body,q){if(q&&!await sure(q))return;const m=document.getElementById('gm_msg');
  try{const r=await j(url,mk(body));say(m,' ✓ '+(r.status||'saved'));}catch(e){say(m,' ✗ '+e.message);}loadGames();}
const saveGames=()=>gamesCall('/api/games/disk',{name:v('gm_name'),letter:v('gm_letter'),size_gb:+v('gm_size'),groups:[v('gm_groups')],volume:v('gm_vol')||null});
const setGamesUpdate=(name,el)=>gamesCall('/api/games/update',{name,id:el.value?+el.value:null});
const toggleGamesRam=(name,ram)=>gamesCall('/api/games/cache',{name,ram},ram?'Serve iSCSI disk '+name+' from RAM? It is loaded in the background (served from the disk meanwhile) and needs about as much free RAM as the data on it; not enough RAM → it stays on the disk (see the log).':null);
const saveGamesUpdate=name=>gamesCall('/api/games/save',{name},'Save the update of '+name+' as its new version? Its update machine must be shut down. Each machine gets it at its next boot.');
const discardGamesUpdate=name=>gamesCall('/api/games/discard',{name},'Throw the update of '+name+' away?');
const delGames=name=>gamesCall('/api/games/delete',{name},'Delete iSCSI disk '+name+' and EVERYTHING on it? Machines using it keep it until they reboot.');
function editGames(name){const d=gmDisks.find(x=>x.disk.name===name).disk;
  for(const [k,val] of [['gm_name',d.name],['gm_letter',d.letter],['gm_size',d.size_gb],['gm_groups',d.groups.join(', ')]])document.getElementById(k).value=val;}

// ---- groups (Groups page) ----
let grData=null,grCur=null;
const grIn=(x,n)=>x.groups.some(y=>y.toLowerCase()===n.toLowerCase());
const grNames=l=>l.length?l.map(x=>esc(x.name)+(x.letter?' ('+esc(x.letter)+':)':'')).join(', '):'—';
async function loadGroups(){const g=grData=await j('/api/groups');const tb=document.querySelector('#groups tbody');if(!tb)return;
  const cnt=n=>g.machines.filter(m=>(m.grp||'').toLowerCase()===n.toLowerCase()).length;
  tb.innerHTML=g.groups.map(n=>`<tr><td><b>${esc(n)}</b></td><td class="mono">${cnt(n)}</td>
      <td>${grNames(g.images.filter(x=>grIn(x,n)))}</td><td>${grNames(g.disks.filter(x=>grIn(x,n)))}</td><td>${grNames(g.drivers.filter(x=>grIn(x,n)))}</td>
      <td><div class="d-flex gap-1"><button class="btn btn-sm btn-outline-secondary" ${act('editGroup',n)}>Edit</button> <button class="btn btn-sm btn-outline-secondary" ${act('renameGroup',n)}>Rename</button> <button class="btn btn-sm btn-outline-danger" ${act('deleteGroup',n)}>Delete</button></div></td></tr>`).join('')
    +`<tr><td><i>every machine</i></td><td class="mono" title="machines without a group">${g.machines.filter(m=>!m.grp).length} without</td>
      <td>${grNames(g.images.filter(x=>!x.groups.length))}</td><td>${grNames(g.disks.filter(x=>!x.groups.length))}</td><td>${grNames(g.drivers.filter(x=>x.all))}</td><td></td></tr>`;
  if(grCur&&g.groups.some(n=>n===grCur))editGroup(grCur);else closeGroup();}
function editGroup(n){if(!grData)return;grCur=n;const g=grData,lc=n.toLowerCase();
  document.getElementById('gr_edit').style.display='';document.getElementById('gr_title').textContent='Group '+n;document.getElementById('gr_emsg').textContent='';
  const box=(v,on,label,off,title)=>`<label class="form-check text-nowrap"${title?` title="${esc(title)}"`:''}><input class="form-check-input" type="checkbox" data-v="${esc(String(v))}"${on?' checked':''}${off?' disabled':''}> ${label}</label>`;
  const all=' <span class="mono">(every machine)</span>',allT='for every machine — limit it to groups on its own page';
  document.getElementById('gr_machines').innerHTML=g.machines.map(m=>box(m.id,(m.grp||'').toLowerCase()===lc,
    esc(m.name)+(m.grp&&m.grp.toLowerCase()!==lc?` <span class="mono">(${esc(m.grp)})</span>`:''))).join('')||'—';
  document.getElementById('gr_images').innerHTML=g.images.map(i=>box(i.id,!i.groups.length||grIn(i,n),
    esc(i.name)+' '+osPill(i.os)+(i.groups.length?'':all),!i.groups.length,i.groups.length?'':allT)).join('')||'—';
  document.getElementById('gr_disks').innerHTML=g.disks.map(d=>box(d.name,!d.groups.length||grIn(d,n),
    esc(d.name)+' ('+esc(d.letter)+':)'+(d.groups.length?'':all),!d.groups.length,d.groups.length?'':allT)).join('')||'—';
  document.getElementById('gr_drivers').innerHTML=g.drivers.map(d=>box(d.id,d.all||grIn(d,n),
    esc(d.name)+(d.all?all:''),d.all,d.all?'for every machine — change it on the Drivers page':'')).join('')||'—';
  document.getElementById('gr_hide').checked=(g.hide_disks||[]).some(x=>x.toLowerCase()===lc);}
function closeGroup(){grCur=null;const e=document.getElementById('gr_edit');if(e)e.style.display='none';}
async function saveGroup(){const m=document.getElementById('gr_emsg');
  const ids=(id,num)=>[...document.querySelectorAll('#'+id+' input:checked:not(:disabled)')].map(e=>num?+e.dataset.v:e.dataset.v);
  const body={name:grCur,machines:ids('gr_machines',1),images:ids('gr_images',1),disks:ids('gr_disks',0),drivers:ids('gr_drivers',1),hide_disks:document.getElementById('gr_hide').checked};
  try{await j('/api/groups/set',mk(body));say(m,' ✓ saved');}
  catch(e){if(/EVERY machine/.test(e.message)&&await sure(e.message+'.\n\nSave anyway?')){
      try{await j('/api/groups/set',mk({...body,allow_all:true}));say(m,' ✓ saved');}catch(e2){say(m,' ✗ '+e2.message);}}
    else say(m,' ✗ '+e.message);}
  loadGroups();}
async function grCall(url,body,el){const m=document.getElementById(el||'gr_msg');
  try{await j(url,mk(body));say(m,' ✓ done');}catch(e){say(m,' ✗ '+e.message);}loadGroups();}
const createGroup=()=>{const n=v('gr_new');if(n)grCall('/api/groups/create',{name:n}).then(()=>{document.getElementById('gr_new').value='';});};
async function renameGroup(n){const to=await ask('Rename group '+n,'New name (an existing name merges the two):',n);
  if(to&&to!==n){if(grCur===n)grCur=to.trim();grCall('/api/groups/rename',{from:n,to});}}
async function deleteGroup(n){if(await sure('Delete group '+n+'? Its machines get no group; it leaves every image, iSCSI disk and driver package.'))grCall('/api/groups/delete',{name:n});}

// ---- system ----
async function loadCafe(){
  j('/api/config').then(c=>{const t=document.getElementById('timeout'),z=document.getElementById('zram_reserve');
    if(t)t.value=c.boot_timeout;if(z)z.value=c.zram_reserve_mb;
    const al=document.getElementById('alert_pct');if(al)al.value=c.alert_pct;
    for(const k of ['room','warn','reboot']){const e=document.getElementById('ssd_'+k);if(e)e.value=c['ssd_'+k+'_gb'];}
    showGoldenStreams(c);}).catch(()=>{});
  const c=await j('/api/cafe-user');
  const u=document.getElementById('cafe_user');if(u)u.value=c.user||'';
  const p=document.getElementById('cafe_password');if(p)p.placeholder=c.password_set?'(unchanged)':'set a password';}
async function saveCafe(){const m=document.getElementById('cafe_msg');
  // Password is write-only: send it only when the field is filled (blank = keep the stored one).
  const body={user:v('cafe_user')},pw=document.getElementById('cafe_password').value;if(pw)body.password=pw;
  try{await j('/api/cafe-user',mk(body));say(m,' ✓ saved');document.getElementById('cafe_password').value='';loadCafe();}
  catch(e){say(m,' ✗ '+e.message);}}
async function savePw(){const m=document.getElementById('pw_msg'),g=id=>document.getElementById(id).value;
  if(g('pw_new').length<8){say(m,' ✗ at least 8 characters');return;}
  if(g('pw_new')!==g('pw_new2')){say(m,' ✗ passwords do not match');return;}
  try{await j('/api/password',mk({current:g('pw_cur'),new:g('pw_new')}));say(m,' ✓ changed, your other sessions signed out');
    for(const id of ['pw_cur','pw_new','pw_new2'])document.getElementById(id).value='';}
  catch(e){say(m,' ✗ '+e.message);}}
// ---- accounts (users.rs; owners): who is signed in decides what the page shows (body[data-role]) ----
let me={};
async function loadMe(){try{me=await j('/api/auth/status');}catch(e){return;}
  document.body.dataset.role=me.role||'';
  const w=document.getElementById('whoami');if(w)w.textContent=me.user?`${me.user} · ${me.role}`:'';}
async function loadUsers(){if(!me.role)await loadMe();
  const tb=document.querySelector('#users tbody');if(!tb||me.role!=='owner')return;
  const u=await j('/api/users');
  tb.innerHTML=u.map(x=>`<tr><td class="mono">${esc(x.name)}${x.name===me.user?' <span class="badge text-bg-secondary">you</span>':''}</td>
    <td><select class="form-select form-select-sm w-auto" data-change="userRole" data-args="${esc(JSON.stringify([x.name]))}">${['owner','staff'].map(r=>`<option${r===x.role?' selected':''}>${r}</option>`).join('')}</select></td>
    <td><div class="d-flex gap-1"><button class="btn btn-sm btn-outline-secondary" ${act('userPw',x.name,x.role)}>New password</button>${x.name===me.user?'':` <button class="btn btn-sm btn-outline-danger" ${act('userDel',x.name)}>Delete</button>`}</div></td></tr>`).join('');}
async function userCall(url,body,done){const m=document.getElementById('us_msg');
  try{await j(url,mk(body));say(m,' ✓ '+done);}catch(e){say(m,' ✗ '+e.message);}loadUsers();}
async function userAdd(){const name=v('us_name').toLowerCase(),p=document.getElementById('us_pw');
  await userCall('/api/users',{name,role:document.getElementById('us_role').value,password:p.value},name+' saved');p.value='';}
const userRole=(name,el)=>userCall('/api/users',{name,role:el.value},name+' is now '+el.value);
async function userPw(name,role){const p=await ask('New password for '+name,'At least 8 characters. Their open sessions end.','');
  if(p!=null)userCall('/api/users',{name,role,password:p},'password of '+name+' changed');}
async function userDel(name){if(await sure('Delete account '+name+'? Their open sessions end.'))userCall('/api/users/delete',{name},name+' deleted');}
async function setTimeout_(){await j('/api/config/timeout',mk({seconds:+document.getElementById('timeout').value}));say(document.getElementById('to_msg'),' ✓ saved');}
async function setSsd(){const m=document.getElementById('ssd_msg');
  try{await j('/api/config/ssd',mk({room_gb:+v('ssd_room'),warn_gb:+v('ssd_warn'),reboot_gb:+v('ssd_reboot')}));say(m,' ✓ saved');}
  catch(e){say(m,' ✗ '+e.message);}}
// Golden download queue: "auto" (number from the link speed) or the admin's number; 0 = no queue.
let gsAuto=8;
function showGoldenStreams(c){const sel=document.getElementById('gs_mode');if(!sel)return;
  gsAuto=c.golden_streams_auto;const auto=c.golden_streams==='auto';sel.value=auto?'auto':'manual';
  document.getElementById('gs_n').value=auto?gsAuto:c.golden_streams;gsMode();
  document.getElementById('gs_info').textContent=`Auto: ${gsAuto} (${c.golden_link||'no network interface found'}). Now: ${c.golden_active} downloading, ${c.golden_waiting} waiting.`;}
function gsMode(){const auto=document.getElementById('gs_mode').value==='auto',n=document.getElementById('gs_n');
  n.disabled=auto;if(auto)n.value=gsAuto;}
async function setGoldenStreams(){const m=document.getElementById('gs_msg');
  const value=document.getElementById('gs_mode').value==='auto'?'auto':String(+v('gs_n'));
  try{await j('/api/config/golden-streams',mk({value}));say(m,' ✓ saved');}catch(e){say(m,' ✗ '+e.message);}}
async function setAlert(){const m=document.getElementById('al_msg');
  try{await j('/api/config/alert',mk({pct:+document.getElementById('alert_pct').value}));say(m,' ✓ saved');}catch(e){say(m,' ✗ '+e.message);}}
async function setZramReserve(){await j('/api/config/zram-reserve',mk({mb:+document.getElementById('zram_reserve').value}));say(document.getElementById('zr_msg'),' ✓ saved');}

// ---- logs (logbuf.rs): the server's newest log lines; Follow asks for the new ones every 3 s ----
let lgLast=0,lgTimer=null,lgSearch=null;
async function loadLogs(){logsReload();logsFollow();}
function logsReload(){lgLast=0;const o=document.getElementById('lg_out');if(o)o.textContent='';logsFetch(true);}
function logsSearch(){clearTimeout(lgSearch);lgSearch=setTimeout(logsReload,300);}
async function logsFetch(first){const o=document.getElementById('lg_out');if(!o)return;
  const p=new URLSearchParams({after:lgLast,level:document.getElementById('lg_level').value,q:document.getElementById('lg_q').value.trim()});
  let r;try{r=await j('/api/logs?'+p);}catch(e){return;}
  const atEnd=o.scrollTop+o.clientHeight>=o.scrollHeight-20;
  for(const l of r.lines){const s=document.createElement('span');s.textContent=l.text+'\n';
    const head=l.text.slice(0,48);if(head.includes(' ERROR '))s.className='text-danger';else if(head.includes(' WARN '))s.className='text-warning';
    o.appendChild(s);}
  while(o.childNodes.length>2000)o.removeChild(o.firstChild);   // the page holds no more than the server keeps
  lgLast=r.last;
  document.getElementById('lg_n').textContent=o.childNodes.length+' lines';
  if(first||atEnd)o.scrollTop=o.scrollHeight;}   // stay at the bottom unless the reader scrolled up
function logsFollow(){clearInterval(lgTimer);lgTimer=null;
  if(!document.getElementById('lg_follow')?.checked)return;
  lgTimer=setInterval(()=>{if(curPage!=='logs'){clearInterval(lgTimer);lgTimer=null;return;}if(!document.hidden)logsFetch(false);},3000);}

// ---- remote (vnc.rs): watch + control a Windows machine over a WebSocket ----
let rmWs=null, rmCtx=null, rmW=0, rmH=0;
const rmProto=()=>location.protocol==='https:'?'wss:':'ws:';
async function loadRemote(){
  try{rmAgents=await j('/api/vnc/agents');}catch(e){rmAgents=[];}
  document.getElementById('rm_hosts').innerHTML=rmAgents.map(a=>`<option value="${esc(a.host)}">`).join('');
  // Group filter: the groups of the machines with an agent (the choice is kept when still there).
  const gs=document.getElementById('rm_wgrp'),cur=gs.value||rmWallGrp;
  const groups=[...new Set(rmAgents.map(a=>a.grp).filter(Boolean))].sort();
  gs.innerHTML='<option value="">All</option>'+groups.map(g=>`<option value="${esc(g)}">${esc(g)}</option>`).join('');
  gs.value=groups.includes(cur)?cur:'';rmWallGrp=gs.value;
  rmWallSizeInit();
  const q=new URLSearchParams(location.search).get('host');
  if(q){openOne(q);}else{showWall();}
}
// Wall: a low-rate thumbnail of every machine running the agent, page by page; click one to control it. Only the
// screens of the page shown are streamed: leaving a page closes its sockets, so those agents stop capturing and the
// browser decodes nothing for them. A hidden tab streams nothing either.
let rmAgents=[], rmWall=[], rmWallPg=1, rmWallGrp='', rmWallN=12, rmWallGen=0;
function closeWall(){rmWallGen++;for(const w of rmWall){w.onclose=null;w.onerror=null;w.close();}rmWall=[];}
const rmWallList=()=>rmAgents.filter(a=>!rmWallGrp||a.grp===rmWallGrp);
function rmWallSizeInit(){
  let n=12;try{n=+localStorage.getItem('rm_wall_n')||12;}catch(e){}
  rmWallN=Math.min(200,Math.max(1,n));
  const sel=document.getElementById('rm_wsize'),inp=document.getElementById('rm_wsize_n');
  const preset=[...sel.options].some(o=>o.value===String(rmWallN));
  sel.value=preset?String(rmWallN):'custom';inp.value=rmWallN;inp.classList.toggle('d-none',preset);
}
function rmWallSize(){
  const sel=document.getElementById('rm_wsize'),inp=document.getElementById('rm_wsize_n');
  const custom=sel.value==='custom';inp.classList.toggle('d-none',!custom);
  const n=Math.min(200,Math.max(1,Math.round(+(custom?inp.value:sel.value)||12)));
  if(custom)inp.value=n;
  rmWallN=n;try{localStorage.setItem('rm_wall_n',String(n));}catch(e){}
  rmWallPg=1;showWall();
}
function rmWallFilter(){rmWallGrp=document.getElementById('rm_wgrp').value;rmWallPg=1;showWall();}
function rmWallPrev(){if(rmWallPg>1){rmWallPg--;showWall();}}
function rmWallNext(){if(rmWallPg<Math.ceil(rmWallList().length/rmWallN)){rmWallPg++;showWall();}}
function showWall(){
  closeWall();rmClose(true);
  document.getElementById('rm_one').classList.add('d-none');
  document.getElementById('rm_wall_box').classList.remove('d-none');
  const list=rmWallList(),pages=Math.max(1,Math.ceil(list.length/rmWallN));
  if(rmWallPg>pages)rmWallPg=pages;
  const first=(rmWallPg-1)*rmWallN,page=list.slice(first,first+rmWallN);
  document.getElementById('rm_wall_n').textContent=rmAgents.length?'('+rmAgents.length+')':'';
  document.getElementById('rm_wall_empty').classList.toggle('d-none',rmAgents.length>0);
  document.getElementById('rm_wpage').textContent=list.length?`${first+1}–${first+page.length} of ${list.length}`:'0';
  const grid=document.getElementById('rm_wall');grid.innerHTML='';
  if(document.hidden)return;   // nothing streams in a hidden tab; shown again → visibilitychange redraws
  const gen=rmWallGen;
  for(const a of page){
    const fig=document.createElement('figure');fig.className='m-0 text-center position-relative';fig.style.cursor='pointer';
    fig.title='Control '+a.host;fig.onclick=()=>remoteTo(a.host);
    const cv=document.createElement('canvas');cv.width=320;cv.height=180;
    cv.className='border rounded';cv.style.width='320px';cv.style.maxWidth='100%';cv.style.height='auto';cv.style.background='#111';
    const note=document.createElement('div');note.className='position-absolute top-50 start-50 translate-middle small text-white d-none';
    note.textContent='in use';
    const cap=document.createElement('figcaption');cap.className='small mono mt-1';cap.textContent=a.host;
    fig.append(cv,note,cap);grid.appendChild(fig);
    if(a.busy)wallBusy(a.host,cv,note,gen);else wallTile(a.host,cv,note,gen);
  }
}
// Someone controls this machine (or it refused the thumbnail): say so, try again in 15 s while this page is shown.
function wallBusy(host,cv,note,gen){note.classList.remove('d-none');
  setTimeout(()=>{if(gen===rmWallGen&&!document.hidden){note.classList.add('d-none');wallTile(host,cv,note,gen);}},15000);}
// One thumbnail stream: low quality, scaled to 320px wide, acked slowly so it refreshes ~1/s.
function wallTile(host,cv,note,gen){
  const ws=new WebSocket(`${rmProto()}//${location.host}/api/vnc/view?host=${encodeURIComponent(host)}&q=35&w=320`);
  ws.binaryType='arraybuffer';rmWall.push(ws);
  let ctx=null;
  ws.onmessage=ev=>{const b=new Uint8Array(ev.data);
    if(b[0]===2){cv.width=b[1]|b[2]<<8;cv.height=b[3]|b[4]<<8;ctx=cv.getContext('2d');return;}
    if(b[0]===1&&ctx){const x=b[1]|b[2]<<8,y=b[3]|b[4]<<8;
      createImageBitmap(new Blob([b.subarray(9)],{type:'image/jpeg'})).then(bm=>{ctx.drawImage(bm,x,y);bm.close();
        setTimeout(()=>{if(ws.readyState===1)ws.send('{"op":"ack"}');},1000);}).catch(()=>{});}};
  // Refused (in use) or taken over by a control view: still on this page → "in use" and retry.
  ws.onclose=()=>{rmWall=rmWall.filter(x=>x!==ws);if(gen===rmWallGen)wallBusy(host,cv,note,gen);};
  ws.onerror=()=>{};
}
// Hidden tab: close the wall's streams; shown again: reopen the current page.
document.addEventListener('visibilitychange',()=>{
  if(curPage!=='remote'||document.getElementById('rm_wall_box')?.classList.contains('d-none'))return;
  if(document.hidden)closeWall();else showWall();
});
// Machines page or a wall tile: open the Remote page controlling this host.
function remoteTo(host){history.pushState(null,'','/remote?host='+encodeURIComponent(host||''));showPage('remote');}
function openOne(host){
  closeWall();
  document.getElementById('rm_wall_box').classList.add('d-none');
  document.getElementById('rm_one').classList.remove('d-none');
  document.getElementById('rm_host').value=host;
  rmConnect();
}
function rmBack(){history.pushState(null,'','/remote');loadRemote();}
// Wall: open a machine by its hostname (typed or picked from the list).
function rmGo(){const h=v('rm_go');if(h)remoteTo(h);}
function rmSay(m){const e=document.getElementById('rm_msg');if(e)say(e,m);}
function rmClose(silent){if(rmWs){rmWs.onclose=null;rmWs.close();rmWs=null;}rmCtx=null;rmCloseDec();
  const c=document.getElementById('rm_canvas');if(c)c.classList.add('d-none');if(!silent)rmSay(' disconnected');}
let rmDec=null;   // WebCodecs VideoDecoder while the agent streams H.264
function rmCloseDec(){if(rmDec){try{rmDec.close();}catch(e){}rmDec=null;}}
function rmConnect(){
  rmClose(true);
  const host=document.getElementById('rm_host').value.trim();if(!host){rmSay(' ✗ enter a hostname');return;}
  const q=+document.getElementById('rm_q').value;
  // Ask for H.264 when the browser can decode it (Chrome/Edge): smooth, catches full-screen games well. The agent
  // falls back to JPEG (and says so with a codec message) if it cannot encode.
  const h264='VideoDecoder'in window;
  const ws=new WebSocket(`${rmProto()}//${location.host}/api/vnc/view?host=${encodeURIComponent(host)}&q=${q}${h264?'&codec=h264':''}`);
  ws.binaryType='arraybuffer';rmWs=ws;rmSay(' connecting…');
  const canvas=document.getElementById('rm_canvas');canvas.style.cursor='';
  const ack=()=>{if(rmWs===ws&&ws.readyState===1)ws.send('{"op":"ack"}');};
  ws.onopen=()=>{rmSay(' connected');canvas.classList.remove('d-none');canvas.focus();};
  ws.onclose=()=>{if(rmWs===ws){rmWs=null;rmCloseDec();rmSay(' ✗ connection closed (agent gone or another viewer took over)');canvas.classList.add('d-none');}};
  ws.onerror=()=>rmSay(' ✗ could not connect (no agent for that host?)');
  ws.onmessage=ev=>{
    const b=new Uint8Array(ev.data);
    if(b[0]===2){rmW=b[1]|b[2]<<8;rmH=b[3]|b[4]<<8;canvas.width=rmW;canvas.height=rmH;rmCtx=canvas.getContext('2d');return;}
    if(b[0]===5){ // the machine's cursor shape: drawn by the browser, so it follows the mouse at once (no second,
      // lagging cursor in the picture). [5] alone = hidden (a game hides it).
      if(b.length<6){canvas.style.cursor='none';return;}
      const hx=b[1]|b[2]<<8,hy=b[3]|b[4]<<8;let s='';for(const c of b.subarray(5))s+=String.fromCharCode(c);
      canvas.style.cursor=`url(data:image/png;base64,${btoa(s)}) ${hx} ${hy}, default`;return;}
    if(b[0]===4){ // codec decision: 0 JPEG, 1 H.264
      rmCloseDec();
      if(b[1]===1){rmDec=new VideoDecoder({
        output:f=>{if(rmCtx)rmCtx.drawImage(f,0,0);f.close();ack();},
        error:e=>{rmSay(' ✗ decode error: '+e.message);rmCloseDec();}});
        rmDec.configure({codec:'avc1.42E01E',optimizeForLatency:true});}
      return;}
    if(b[0]===3&&rmDec){ // H.264 access unit, b[1] bit0 = keyframe
      try{rmDec.decode(new EncodedVideoChunk({type:(b[1]&1)?'key':'delta',timestamp:performance.now()*1000,data:b.subarray(2)}));}
      catch(e){ack();}
      return;}
    if(b[0]===1&&rmCtx){
      const x=b[1]|b[2]<<8,y=b[3]|b[4]<<8;
      createImageBitmap(new Blob([b.subarray(9)],{type:'image/jpeg'})).then(bm=>{
        if(rmCtx)rmCtx.drawImage(bm,x,y);bm.close();ack();
      }).catch(ack);
    }
  };
  // Mouse: canvas pixels -> screen pixels (canvas is scaled by CSS). b = button bitmask.
  const send=o=>{if(rmWs===ws&&ws.readyState===1)ws.send(JSON.stringify(o));};
  const pos=e=>{const r=canvas.getBoundingClientRect();
    return[Math.round((e.clientX-r.left)/r.width*rmW),Math.round((e.clientY-r.top)/r.height*rmH)];};
  const btn=e=>(e.buttons&1)|(e.buttons&2)|(e.buttons&4); // same bit layout as Windows
  canvas.onmousemove=e=>{const[x,y]=pos(e);send({op:'mouse',x,y,b:btn(e)});};
  canvas.onmousedown=e=>{e.preventDefault();canvas.focus();const[x,y]=pos(e);send({op:'mouse',x,y,b:btn(e)});};
  canvas.onmouseup=e=>{const[x,y]=pos(e);send({op:'mouse',x,y,b:btn(e)});};
  canvas.oncontextmenu=e=>e.preventDefault();
  canvas.onwheel=e=>{e.preventDefault();send({op:'wheel',d:e.deltaY<0?1:-1});};
  canvas.onkeydown=e=>{e.preventDefault();send({op:'key',code:e.code,down:true});};
  canvas.onkeyup=e=>{e.preventDefault();send({op:'key',code:e.code,down:false});};
}
function rmSend(o){if(rmWs&&rmWs.readyState===1)rmWs.send(JSON.stringify(o));}
function rmQuality(){rmSend({op:'start',q:+document.getElementById('rm_q').value});}
// Send a chord: press the codes in order, release in reverse. The buttons work even when the browser won't let the
// page capture these keys from the physical keyboard (only full screen does).
function rmChord(codes){for(const c of codes)rmSend({op:'key',code:c,down:true});
  for(let i=codes.length-1;i>=0;i--)rmSend({op:'key',code:codes[i],down:false});}
function rmSas(){rmSend({op:'sas'});}
function rmWin(){rmChord(['MetaLeft']);}
function rmAltTab(){rmChord(['AltLeft','Tab']);}
async function rmPaste(){
  let text='';
  try{text=await navigator.clipboard.readText();}       // needs a secure context; LAN http usually lacks it
  catch(e){text=prompt('Clipboard not available in the browser here. Paste the text to send:')||'';}
  if(!text)return;
  rmSend({op:'clip',text});                               // set the machine's clipboard, then Ctrl+V on it
  setTimeout(()=>rmChord(['ControlLeft','KeyV']),150);
  rmSay(' pasted '+text.length+' chars');
}
function rmFull(){const c=document.getElementById('rm_canvas');if(!c)return;
  if(c.requestFullscreen)c.requestFullscreen().then(()=>{if(navigator.keyboard&&navigator.keyboard.lock)navigator.keyboard.lock();}).catch(()=>{});}
// Leaving the Remote page closes the socket (showPage calls this via the leave hook below).

// ---- server liveness (SSE /api/events) ----
// Any event counts: "ping" comes on connect and only after 5 s without other events (axum's keep-alive restarts
// its timer on every event, and "res" arrives every 2 s). Red on connection error or nothing for 12 s (e.g. server
// powered off, half-open link). EventSource reconnects by itself → green again on the next event.
let lastPing=0;
const alive=()=>{lastPing=Date.now();setServer(true);};
function setServer(up){const d=document.getElementById('srvdot');
  d.classList.toggle('up',up);d.classList.toggle('down',!up);d.title=up?'server online':'server offline';}

// ---- benchmark (bench.rs) ----
const BENCH_KINDS={srv_seq:['Server sequential','MB/s'],srv_rand:['Server random 4K','IOPS'],disk:['Disk read','MB/s'],net:['Network','MB/s'],boot:['Boot','s']};
let benchData=null,iscsiES=null;
const bName=mac=>{if(!mac)return 'server';const m=benchData&&benchData.machines.find(x=>x.mac===mac);return (m&&m.hostname)||mac;};
const bVal=r=>r?`${r.value.toLocaleString()} ${BENCH_KINDS[r.kind]?BENCH_KINDS[r.kind][1]:''}`:'—';
// Value + a bar against the best machine (higher = better: disk, network).
const bBar=(r,max)=>r?`${esc(bVal(r))}<div class="progress d-inline-flex align-middle ms-2" style="width:70px;height:6px"><div class="progress-bar" style="width:${Math.round(100*r.value/max)}%"></div></div>`:'—';
const bTime=t=>new Date(t*1000).toLocaleString();
async function loadBench(){
  const d=benchData=await j('/api/bench');
  const seq=document.getElementById('b_seq');if(!seq)return;
  const last=(mac,k)=>d.latest.find(r=>r.mac===mac&&r.kind===k);
  seq.textContent=bVal(last(null,'srv_seq'));document.getElementById('b_rand').textContent=bVal(last(null,'srv_rand'));
  if(d.job&&d.job.startsWith('⏳')&&!jobWatch['bench:server'])watchJob('bench:server',document.getElementById('bench_msg'),benchDone);
  const max=k=>Math.max(1,...d.latest.filter(r=>r.mac&&r.kind===k).map(r=>r.value));
  // Registered machines + any other MAC with results.
  const macs=[...new Set([...d.machines.map(m=>m.mac),...d.latest.filter(r=>r.mac).map(r=>r.mac)])];
  const checked=new Set([...document.querySelectorAll('.b_sel:checked')].map(x=>x.value));
  document.querySelector('#b_clients tbody').innerHTML=macs.map(mac=>{
    const boot=last(mac,'boot'),disk=last(mac,'disk'),net=last(mac,'net');
    const at=Math.max(0,...[boot,disk,net].filter(Boolean).map(r=>r.at));
    const stage=boot&&/stage (\d+)s/.exec(boot.detail);
    return `<tr><td><input type="checkbox" class="b_sel form-check-input" value="${esc(mac)}"${checked.has(mac)?' checked':''}></td>
      <td><b>${esc(bName(mac))}</b> <span class="mono">${esc(mac)}</span>${d.pending.includes(mac)?' <span class="badge rounded-pill text-bg-warning" title="measured at its next boot">waiting</span>':''}</td>
      <td title="${esc(boot?boot.detail:'')}">${esc(bVal(boot))}${stage?` <span class="mono">(stage ${esc(stage[1])} s)</span>`:''}</td>
      <td title="${esc(disk?disk.detail:'')}">${bBar(disk,max('disk'))}</td><td>${bBar(net,max('net'))}</td>
      <td class="mono">${at?esc(bTime(at)):'—'}</td></tr>`;}).join('')||'<tr><td colspan=6 class="mono">no machines yet</td></tr>';
  const sel=document.getElementById('b_hmac'),cur=sel.value;
  sel.innerHTML='<option value="">all</option><option value="server">server</option>'+macs.map(m=>`<option value="${esc(m)}">${esc(bName(m))}</option>`).join('');
  sel.value=cur;
  loadBenchHist();
}
// Server page: live load + disk health ("res" SSE), iSCSI now, and its alert / RAM-reserve settings.
function loadServer(){loadIscsi();if(lastRes)showRes(lastRes);loadCafe();drawCharts();loadServices();loadServerDisks();
  if(!svcTimer)svcTimer=setInterval(()=>{if(curPage!=='server'){clearInterval(svcTimer);svcTimer=null;return;}loadServices();},10000);}
// Services (sysmon.rs /api/server/status): each one up or down, refreshed every 10 s while the page is open.
let svcTimer=null;
async function loadServices(){const el=document.getElementById('b_svc');if(!el)return;
  let s;try{s=await j('/api/server/status');}catch(e){el.textContent='✗ '+e.message;return;}
  // ok: true running · false down · null off (by choice) · a string = a plain label.
  const row=(name,ok,detail)=>`<tr><td>${name}</td><td><span class="badge ${ok===true?'text-bg-success':ok===false?'text-bg-danger':'text-bg-secondary'}">${typeof ok==='string'?esc(ok):ok===true?'running':ok===false?'down':'off'}</span></td><td class="mono">${esc(detail)}</td></tr>`;
  const i=s.iscsi;
  el.innerHTML=`<table class="table table-sm w-auto mb-0"><tbody>
    ${row('Web admin + boot HTTP',true,`bootrom-mgmt up ${dur(s.uptime)}`+(s.host_uptime!=null?` · server up ${dur(s.host_uptime)}`:''))}
    ${s.dhcp.mode==='full'?row('DHCP server',s.dhcp.running,`:67 on ${s.dhcp.iface||'?'}`+(s.dhcp.running?'':' — could not bind (see Logs)')):row('DHCP server',null,'the LAN\'s own DHCP hands out addresses')}
    ${row('TFTP (iPXE)',s.tftp.running,':69')}
    ${row('iSCSI target',!!i,i?`${i.targets} targets, ${i.sessions} sessions`+(i.clients!=null?`, ${i.clients} clients`:''):'daemon not running')}
    ${s.dns&&s.dns.on?row('DNS (healthy machines)',s.dns.running,s.dns.running?`:53 ${s.dns.recursive?'recursive, '+s.dns.cached+' cached':'passthrough → '+s.dns.upstream.join(', ')} · ${s.dns.allowed} IPs may resolve`:'could not start (see Logs)'):''}
    ${row('Remote agents',String(s.agents),'Windows machines connected')}
    ${row('Golden downloads',String(s.golden.running),`running, ${s.golden.waiting} waiting their turn`)}
  </tbody></table>`;}

// Charts (Chart.js, vendored: chart.umd.min.js). The tab keeps the last RES_KEEP "res" samples (10 min at 2 s), across
// page changes; they are drawn only while the Server page shows.
const RES_KEEP=300, resHist=[], charts={};
const fmtNum=v=>String(Number(v.toFixed(v>=10?0:v>=1?1:2)));
// Per canvas: its lines (sample field, label, Bootstrap color variable) and y axis (max = fixed top, unit).
const CHARTS={
  ch_cpu:[[['cpu','CPU','--bs-primary'],['ram','RAM','--bs-success']],{max:100,unit:'%'}],
  ch_net:[[['tx','out','--bs-primary'],['rx','in','--bs-success'],['iscsi','iSCSI','--bs-warning'],['http','golden HTTP','--bs-danger']],{unit:' MB/s'}],
  ch_disk:[[['busy','busy','--bs-primary']],{max:100,unit:'%'}],
};
// Colors from Bootstrap's variables, read when the chart is made (each visit of the page): light / dark theme follow.
function mkChart(cv,lines,o){
  const css=getComputedStyle(document.documentElement),col=v=>css.getPropertyValue(v).trim()||'#888';
  const fg=col('--bs-secondary-color'),grid=col('--bs-border-color-translucent');
  return new Chart(cv,{type:'line',
    data:{labels:[],datasets:lines.map(([,label,c])=>({label,data:[],borderColor:col(c),backgroundColor:col(c),borderWidth:1.5,pointRadius:0,tension:.2}))},
    options:{animation:false,responsive:true,maintainAspectRatio:false,interaction:{mode:'index',intersect:false},
      plugins:{legend:{labels:{color:fg,boxWidth:10,boxHeight:3}},
        tooltip:{callbacks:{label:c=>`${c.dataset.label}: ${c.parsed.y==null?'—':fmtNum(c.parsed.y)+o.unit}`}}},
      scales:{x:{ticks:{color:fg,maxTicksLimit:6,maxRotation:0},grid:{color:grid}},
        y:{min:0,max:o.max,ticks:{color:fg,maxTicksLimit:5},grid:{color:grid}}}}});}
function drawCharts(){if(curPage!=='server'||typeof Chart==='undefined')return;
  const labels=resHist.map(r=>new Date(r.t).toLocaleTimeString());
  for(const [id,[lines,o]] of Object.entries(CHARTS)){const cv=document.getElementById(id);if(!cv)return;
    let c=charts[id];if(!c||c.canvas!==cv){c?.destroy();c=charts[id]=mkChart(cv,lines,o);} // the page was re-rendered
    c.data.labels=labels;lines.forEach(([k],i)=>{c.data.datasets[i].data=resHist.map(r=>r[k]);});c.update('none');}}
function resSample(d){const mb=b=>b==null?null:b/1e6;
  resHist.push({t:Date.now(),cpu:d.cpu.pct,ram:d.ram.pct,rx:mb(d.net.rx),tx:mb(d.net.tx),
    iscsi:mb(d.egress?.iscsi),http:mb(d.egress?.http),busy:d.disk.busy});
  if(resHist.length>RES_KEEP)resHist.shift();}
async function loadBenchHist(){
  const sel=document.getElementById('b_hmac');if(!sel)return;
  const rows=await j('/api/bench/history?mac='+encodeURIComponent(sel.value));
  document.querySelector('#b_hist tbody').innerHTML=rows.map(r=>`<tr><td class="mono">${esc(bTime(r.at))}</td><td>${esc(bName(r.mac))}</td>
    <td>${esc(BENCH_KINDS[r.kind]?BENCH_KINDS[r.kind][0]:r.kind)}</td><td><b>${esc(bVal(r))}</b></td><td class="mono">${esc(r.detail)}</td></tr>`).join('')
    ||'<tr><td colspan=5 class="mono">no results yet</td></tr>';
}
// iSCSI now: pushed every second over SSE while the Server page is open (showPage closes it on leaving).
function loadIscsi(){
  if(iscsiES)return;
  iscsiES=new EventSource('/api/bench/iscsi/live');
  iscsiES.addEventListener('iscsi',e=>showIscsi(JSON.parse(e.data)));
  iscsiES.onerror=()=>{const el=document.getElementById('b_iscsi');
    if(el)el.innerHTML=iscsiES&&iscsiES.readyState===EventSource.CLOSED?`disconnected — ${ic('refresh','')} to reconnect`:'reconnecting...';
    if(iscsiES&&iscsiES.readyState===EventSource.CLOSED)iscsiES=null;};
}
function showIscsi(d){
  const el=document.getElementById('b_iscsi');if(!el)return;
  if(d.daemon==='none'){el.textContent='iSCSI daemon not running';return;}
  if(d.daemon==='old'){el.textContent='the running iSCSI daemon is an older build without counters — it is replaced by itself once no client is connected';return;}
  if(!d.targets.length&&!d.peers.length){el.textContent='no client connected';return;}
  const mb=x=>x.toFixed(1)+' MB/s';
  el.innerHTML=`<div class="d-flex flex-wrap align-items-start gap-4">
    <table class="table table-sm w-auto mb-0"><thead><tr><th>Target</th><th>Sessions</th><th>Read</th></tr></thead><tbody>${d.targets.map(t=>
      `<tr><td>${esc(t.name)}${t.ram?' <span class="badge text-bg-secondary">RAM</span>':''}</td><td>${t.sessions}</td><td>${mb(t.mbps)}</td></tr>`).join('')}</tbody></table>
    <table class="table table-sm w-auto mb-0"><thead><tr><th>Client</th><th>Read</th></tr></thead><tbody>${d.peers.map(p=>
      `<tr><td>${esc(p.host||p.ip)} <span class="mono">${esc(p.ip)}</span></td><td>${p.mbps>0?mb(p.mbps):'<span class="mono">idle</span>'}</td></tr>`).join('')||'<tr><td colspan=2 class="mono">none</td></tr>'}</tbody></table></div>`;
}
// Server resources (sysmon.rs): "res" on /api/events every 2 s → the alert banner (every page) + "Server now".
let lastRes=null;
function showRes(d){lastRes=d;
  const bar=document.getElementById('alertbar');
  if(bar){bar.hidden=!d.alerts.length;
    if(d.alerts.length)bar.innerHTML=`<b>${ic('warn')} Server at its limit:</b> ${esc(d.alerts.join(', '))} (threshold ${d.limit}%) — clients boot and load slower. <a class="alert-link" href="/server" data-click="go" data-args='["server"]'>Details</a>`;}
  const el=document.getElementById('b_res');if(!el)return;
  // Measured values first; a bar + % only where the whole is known (none for a VM's virtual network card).
  const row=(name,pct,txt)=>`<tr><td>${name}</td><td>${pct==null?'':
    `<div class="progress d-inline-flex align-middle me-2" style="width:140px;height:8px"><div class="progress-bar${pct>=d.limit?' bg-danger':''}" style="width:${Math.min(100,pct).toFixed(0)}%"></div></div>${pct.toFixed(0)}%`}</td><td class="mono">${esc(txt)}</td></tr>`;
  const rate=b=>(b/1e6).toFixed(1)+' MB/s';
  const {cpu:c,ram:r,net:n,disk:k,space:s}=d;
  el.innerHTML=`<table class="table table-sm w-auto mb-0"><tbody>
    ${row('CPU',c.pct,`${c.cores} cores${c.load!=null?`, load ${c.load.toFixed(2)} (1 min)`:''}`)}
    ${row('RAM',r.pct,`${gbs(r.used)} used of ${gbs(r.total)} (+ ${gbs(r.cache)} page cache, given back when needed)`)}
    ${row('Network',n.pct,`${n.iface||'?'}: out ${rate(n.tx)}, in ${rate(n.rx)}`+(n.link?`, link ${n.link>=1000?n.link/1000+' Gbit/s':n.link+' Mbit/s'}`:'')
      +(n.virtual?` — virtual card (${n.virtual})`:''))}
    ${row('Disk busy',k.busy,k.dev?`${k.dev}: read ${rate(k.read)}, write ${rate(k.write)}`:'images on a filesystem without a block device')}
    ${row('Disk space',s.pct,s.total?`${gbs(s.used)} used of ${gbs(s.total)} (image storage)`:'image storage: unknown')}
    ${k.health?`<tr><td>Disk health</td><td class="mono ${srvDiskBad(k.health)?'text-danger':''}">${esc(srvDiskHealth(k.health))}</td><td></td></tr>`:''}
  </tbody></table>`;}
const srvDiskBad=h=>/fail/i.test(h.status)||(h.realloc||0)>0||(h.pending||0)>0;
function srvDiskHealth(h){const p=[];
  p.push(h.status||'SMART: no overall result');
  if(h.temp!=null)p.push(h.temp+'°C');
  if(h.realloc!=null||h.pending!=null)p.push('bad sectors '+(h.realloc||0)+' reallocated / '+(h.pending||0)+' pending');
  return p.join(' · ');}
const benchDone=()=>{if(curPage==='benchmark')loadBench();};
async function benchServer(){const el=document.getElementById('bench_msg');
  try{await j('/api/bench/server',{method:'POST'});watchJob('bench:server',el,benchDone);}catch(e){say(el,' ✗ '+e.message);}}
async function benchAsk(cancel){const m=document.getElementById('bq_msg');
  const macs=[...document.querySelectorAll('.b_sel:checked')].map(x=>x.value);
  if(!macs.length){toast('Tick the machines first',true);return;}
  try{await j('/api/bench/request',mk({macs,cancel}));say(m,cancel?' ✓ cancelled':` ✓ ${macs.length} machine(s) measured at their next boot`);}
  catch(e){say(m,' ✗ '+e.message);}
  loadBench();}
function bSelAll(el){document.querySelectorAll('.b_sel').forEach(x=>x.checked=el.checked);}

// ---- helpers ----
function v(id){return document.getElementById(id).value.trim()}
function mk(body){return {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)}}
// Non-blocking notification (Bootstrap toast, bottom right); errors stay longer and in red.
function toast(msg,err){const t=document.getElementById('toast');
  document.getElementById('toast_text').textContent=msg;
  t.classList.toggle('text-danger-emphasis',!!err);t.classList.toggle('bg-danger-subtle',!!err);t.classList.toggle('border-danger',!!err);
  bootstrap.Toast.getInstance(t)?.dispose();   // the delay is fixed per instance
  new bootstrap.Toast(t,{delay:err?6000:3500}).show();}
// The app's dialog (#dlg in index.html) instead of prompt() / confirm(), which a browser lets the user silence.
// ask(): the typed text, or null on Cancel / Esc (multi = a textarea). sure(): true on OK. Both resolve once the
// dialog is fully hidden, so the caller can open it again at once (asking again after a bad value).
function dialog(title,text,value,multi){
  const $=id=>document.getElementById(id),m=$('dlg'),f=$(multi?'dlg_area':'dlg_input');
  $('dlg_title').textContent=title;$('dlg_text').textContent=text||'';$('dlg_text').hidden=!text;
  $('dlg_input').hidden=$('dlg_area').hidden=true;
  if(value!=null){f.hidden=false;f.value=value;}
  return new Promise(done=>{let ok=false;
    $('dlg_form').onsubmit=e=>{e.preventDefault();ok=true;bootstrap.Modal.getInstance(m).hide();};
    m.addEventListener('shown.bs.modal',()=>(value!=null?f:$('dlg_ok')).focus(),{once:true});
    m.addEventListener('hidden.bs.modal',()=>done(value!=null?(ok?f.value:null):ok),{once:true});
    bootstrap.Modal.getOrCreateInstance(m).show();});
}
const ask=(title,text,value,multi)=>dialog(title,text,value??'',multi);
const sure=text=>dialog('Confirm',text);

// ---- admin login (auth.rs) ----
// Login lives on its own page (/login). This page is only served to a signed-in admin: the server redirects an
// unauthenticated browser here → /login. A 401 on any API call (session expired) sends us back to /login.
async function logout(){try{await fetch('/api/auth/logout',{method:'POST'});}catch(e){}location.replace('/login');}

// Everything an element may name in data-click / data-change / data-input (see act()). Called as f(...args, el, event).
const ACTIONS={
  noop(){},   // a cell that must not open its row (checkbox column)
  go(p,el,e){e.preventDefault();history.pushState(null,'','/'+p);showPage(p);},
  href(url){location.href=url;},
  mFilter(){mPage=1;renderMachines();},
  dvToggle:(k,el)=>dvToggle(k,el.checked), dvSelectAll:el=>dvSelectAll(el.checked),
  wake,manage,regMachine,loadStatus,clearMFilters,mPrev,mNext,mPageSize,setKey,rearmKey,
  loadDevices,dvOpen,dvSave,dvDelete,dvClose,dvBulk,dvRegister,dvImport,dvAdd,dvRender,dvHide,dvBroomDisk,
  volMove,volFormat,volFolder,volRemove,loadServerDisks,
  userAdd,userRole,userPw,userDel,rekey,saveDnsGate,dnsResolveShow,
  loadImages,addImage,setDefault,toggleCache,toggleBase,toggleSsd,togglePreload,editImgGroups,republish,showVersions,exportImage,editBoot,delImage,
  snapshotImage,rollbackImage,versionToImage,deleteVersion,
  loadGames,saveGames,setGamesUpdate,toggleGamesRam,saveGamesUpdate,discardGamesUpdate,editGames,delGames,
  loadDrivers,uploadDriver,saveDrvTargets,delDriver,
  loadGroups,editGroup,closeGroup,saveGroup,createGroup,renameGroup,deleteGroup,
  loadServer,loadBench,loadBenchHist,logsReload,logsFollow,logsSearch,benchServer,benchRequest:()=>benchAsk(false),benchCancel:()=>benchAsk(true),bSelAll,
  applyDhcp,savePw,saveCafe,setTimeout_,setZramReserve,setAlert,setSsd,gsMode,setGoldenStreams,logout,
  remoteTo,rmGo,rmConnect,rmClose,rmBack,rmWallSize,rmWallFilter,rmWallPrev,rmWallNext,rmQuality,rmFull,rmSas,rmWin,rmAltTab,rmPaste,loadRemote,
};

let started=false;
function startApp(){
  if(started)return; started=true;
  loadMe();
  const srvEvents=new EventSource('/api/events');
  srvEvents.addEventListener('ping',alive);
  srvEvents.addEventListener('job',e=>{alive();const d=JSON.parse(e.data);showJob(d.name,d.status);});
  srvEvents.addEventListener('res',e=>{alive();const d=JSON.parse(e.data);showRes(d);resSample(d);drawCharts();});
  // The server ended the stream because the session is gone (expired / password changed elsewhere) → sign in again.
  srvEvents.addEventListener('logout',()=>{srvEvents.close();location.replace('/login');});
  srvEvents.onerror=()=>setServer(false);
  // (Re)connected: job events sent while the link was down are lost → ask for the status of every followed job.
  srvEvents.onopen=()=>{for(const n in jobWatch)j('/api/images/job?name='+encodeURIComponent(n)).then(r=>showJob(n,r.status)).catch(()=>{});};
  setInterval(()=>{if(lastPing&&Date.now()-lastPing>12000)setServer(false);},3000);
  if(location.pathname!=='/'+pageFromPath())history.replaceState(null,'','/'+pageFromPath());
  showPage(pageFromPath());
  // Every page shows the online count in the sidebar: keep it fresh everywhere (the Machines table redraws too).
  if(pageFromPath()!=='machines')loadStatus().catch(()=>{});
  setInterval(()=>{if(!document.hidden)loadStatus().catch(()=>{});},15000);
}
startApp();
