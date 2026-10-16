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
const loaders={machines:loadStatus,devices:loadDevices,groups:loadGroups,benchmark:loadBench,images:()=>{loadImages();loadGames();},drivers:loadDrivers,network:loadDhcp,system:loadCafe,remote:loadRemote};
const fragCache={};
let curPage=null;
async function showPage(p){
  if(iscsiES&&p!=='benchmark'){iscsiES.close();iscsiES=null;}
  if(p!=='remote'&&rmWs){rmWs.onclose=null;rmWs.close();rmWs=null;rmCtx=null;}
  document.querySelectorAll('nav a').forEach(x=>x.classList.toggle('active',x.dataset.page===p));
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

// ---- machines (filter + client-side paging) ----
let allMachines=[], mPage=1; const M_PAGE=15;
async function loadStatus(){
  allMachines=await j('/api/status');
  const online=allMachines.filter(m=>m.online).length;
  document.getElementById('onlinecount').textContent=online+'/'+allMachines.length+' online';
  renderMachines();
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
       <td>${led(m.online)}${m.not_reset?' <span class="badge text-bg-danger" title="Windows started from the SSD without a PXE boot at '+new Date(m.not_reset*1000).toLocaleString()+' — this session was NOT reset (cable out, server down, boot order changed?). Cleared by the next PXE boot.">not reset</span>':''}${m.ssd_low?' <span class="badge text-bg-danger" title="SSD almost full during the session: '+gbs(m.ssd_low[0])+' free at '+new Date(m.ssd_low[1]*1000).toLocaleString()+' (the guest wrote a lot; Settings → SSD room). Cleared by the next PXE boot.">SSD full</span>':''}</td>
       <td class="mono" title="${esc(m.license_result||'')}">${licCell(m)}</td>
       <td><div class="d-flex gap-1">${m.registered
          ? `<button class="btn btn-sm btn-outline-secondary" ${act('wake',m.mac)}>Wake</button>
             <button class="btn btn-sm btn-outline-secondary" ${act('manage',m.id)}>Manage</button>
             ${m.remote?`<button class="btn btn-sm btn-outline-primary" ${act('remoteTo',m.hostname)} title="See and control this machine">Remote</button>`:''}`
          : `<button class="btn btn-sm btn-outline-primary" ${act('regMachine',m.mac,m.ip,m.hostname)}>Register</button>`}</div></td>
     </tr>`).join('') || '<tr><td colspan=7 class="mono">no matching machines</td></tr>';
  document.getElementById('m_count').textContent=rows.length+' machines';
  document.getElementById('m_page').textContent='Page '+mPage+'/'+pages;
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
      <tr><td class="mono">License</td><td class="mono" style="white-space:normal">${licCell(m,true)}</td></tr>
      <tr><td class="mono">Driver packages</td><td class="mono">${d.drivers.length?esc(d.drivers.join(', ')):'—'}</td></tr>
      <tr><td class="mono">Hardware (${d.hwids.length})</td><td class="mono" style="white-space:normal">${d.hwids.length
        ?`<details><summary>IDs its stage reported at the last boot</summary>${d.hwids.map(esc).join('<br>')}</details>`
        :'— (no Windows boot since this was added)'}</td></tr>
    </tbody></table>`;
  dvRender();
}
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
let imgRows=[];
async function loadImages(){
  const rows=imgRows=await j('/api/images');
  document.querySelector('#images tbody').innerHTML=rows.map(i=>
    `<tr><td>${esc(i.name)}${i.export?`<div class="mono" style="font-size:12px" title="exported ${i.export.created?new Date(i.export.created*1000).toLocaleString():''} — save both into one folder, open the .vmx in VMware">${ic('down','')} <a href="/api/images/export-file?id=${i.id}&f=vmx">.vmx</a> · <a href="/api/images/export-file?id=${i.id}&f=vmdk">.vmdk ${gbs(i.export.size)}</a> (${esc(i.export.version)})</div>`:''}</td><td>${osPill(i.os)}</td>
       <td><a href="#" ${act('editImgGroups',i.id,i.name,i.groups)} title="machine groups that get it in their boot menu">${i.groups.length?esc(i.groups.join(', ')):'all'}</a></td>
       <td class="mono">${i.cache_mode==='zram'?'RAM':'disk'}</td>
       <td>${i.is_default?ic('ok'):''}</td>
       <td>${i.boot_script?ic('ok'):'<span class="mono">no</span>'}</td>
       <td class="mono" title="used on disk / virtual disk">${i.size==null?'—':gbs(i.used)+' / '+gbs(i.size)}${i.active_version?' <span class="badge rounded-pill text-bg-success">'+esc(i.active_version)+'</span>':''}</td>
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
       </div></td></tr>`).join('') || '<tr><td colspan=9 class="mono">no images yet</td></tr>';
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
  try{await j('/api/images',mk({name,os,cache_mode:cache}));}catch(e){}
  try{await uploadFiles('/api/images',name,files,document.getElementById('up_prog'),msg);}
  catch(e){say(msg,' ✗ '+e.message);return;}
  say(msg,' ✓ upload done');loadImages();watchJob(name,msg);   // convert + publish run in the background → SSE
}

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
async function loadDhcp(){const c=await j('/api/dhcp');
  ['mode','iface','server_ip','subnet','netmask','range_start','range_end','gateway'].forEach(k=>{const e=document.getElementById('dhcp_'+k);if(e)e.value=c[k]||'';});
  for(const k of ['ipxe_signed','strict_reset','rapid_commit','ipxe_fast','authoritative','send_hostname']){const e=document.getElementById('dhcp_'+k);if(e)e.checked=!!c[k];}
  setDns(c.dns);
  const dm=document.getElementById('dhcp_mode');if(dm)dm.onchange=toggleFull;toggleFull();}
function toggleFull(){const dm=document.getElementById('dhcp_mode');if(!dm)return;
  for(const id of ['full_only','full_only_opts']){const e=document.getElementById(id);if(e)e.style.display=dm.value==='full'?'':'none';}}
async function applyDhcp(){const b={};['mode','iface','server_ip','subnet','netmask','range_start','range_end','gateway'].forEach(k=>b[k]=document.getElementById('dhcp_'+k).value);
  for(const k of ['ipxe_signed','strict_reset','rapid_commit','ipxe_fast','authoritative','send_hostname']){const e=document.getElementById('dhcp_'+k);if(e)b[k]=e.checked?'1':'0';}
  b.dns=getDns();
  const m=document.getElementById('dhcp_msg');
  try{const r=await j('/api/dhcp',mk(b));say(m,' ✓ '+(r.status||'applied'));}catch(e){say(m,' ✗ '+e.message);}}

// ---- games disks (Images page) ----
let gmDisks=[];
async function loadGames(){const g=await j('/api/games');const tb=document.querySelector('#games tbody');if(!tb)return;
  gmDisks=g.disks;
  document.getElementById('gm_grouplist').innerHTML=g.groups.map(x=>`<option value="${esc(x)}">`).join('');
  const opts=sel=>'<option value="">— none —</option>'+g.machines.map(m=>
    `<option value="${m.id}"${m.id===sel?' selected':''}>${esc(m.name)}${m.grp?' ('+esc(m.grp)+')':''}</option>`).join('');
  tb.innerHTML=g.disks.map(x=>{const d=x.disk,s=x.status;
    return `<tr><td>${esc(d.name)}</td><td class="mono">${esc(d.letter)}:</td><td class="mono">${d.size_gb} GB</td>
      <td>${d.groups.length?esc(d.groups.join(', ')):'<span class="mono">all</span>'}</td>
      <td class="mono">${s?'g'+s.ver+(s.layers?` <span title="versions not merged into the disk yet (${gbs(s.layer_bytes)})">+${s.layers} ${s.merging?'merging…':'to merge'}</span>`:''):'<span class="text-danger">not served</span>'}</td>
      <td class="mono">${s?s.sessions+(s.old_sessions?` <span title="still on an older version until they reboot">(${s.old_sessions} older)</span>`:''):''}</td>
      <td><select class="form-select form-select-sm d-inline-block w-auto" data-change="setGamesUpdate" data-args="${esc(JSON.stringify([d.name]))}">${opts(d.update)}</select>${d.update!=null&&!x.update_ip?' <span class="text-danger" title="no fixed IP or lease known">no IP</span>':''}${s&&s.update_connected?' <span class="text-success">'+ic('dot','')+' writable</span>':''}</td>
      <td class="mono">${s&&s.update_bytes?gbs(s.update_bytes)+` <button class="btn btn-sm btn-outline-primary" ${act('saveGamesUpdate',d.name)}>Save</button> <button class="btn btn-sm btn-outline-secondary" ${act('discardGamesUpdate',d.name)}>Discard</button>`:'—'}</td>
      <td><button class="btn btn-sm btn-outline-secondary" ${act('toggleGamesRam',d.name,!d.ram)} title="RAM: the disk is served from a copy in the server's RAM (loaded in the background; needs about as much free RAM as the games on it — games barely compress). Disk: read from games.img (the page cache keeps what is played most).">${d.ram?(s&&s.ram?'Cache: RAM':(s&&s.ram_loading?'Cache: RAM (loading…)':'Cache: RAM (not loaded)')):'Cache: disk'}</button> <button class="btn btn-sm btn-outline-secondary" ${act('editGames',d.name)}>Edit</button> <button class="btn btn-sm btn-outline-danger" ${act('delGames',d.name)}>Delete</button></td></tr>`;}).join('')
    ||'<tr><td colspan="9" class="mono">no games disk yet</td></tr>';
  const m=document.getElementById('gm_msg');if(m&&g.error)say(m,' ✗ not served: '+g.error);}
async function gamesCall(url,body,q){if(q&&!await sure(q))return;const m=document.getElementById('gm_msg');
  try{const r=await j(url,mk(body));say(m,' ✓ '+(r.status||'saved'));}catch(e){say(m,' ✗ '+e.message);}loadGames();}
const saveGames=()=>gamesCall('/api/games/disk',{name:v('gm_name'),letter:v('gm_letter'),size_gb:+v('gm_size'),groups:[v('gm_groups')]});
const setGamesUpdate=(name,el)=>gamesCall('/api/games/update',{name,id:el.value?+el.value:null});
const toggleGamesRam=(name,ram)=>gamesCall('/api/games/cache',{name,ram},ram?'Serve games disk '+name+' from RAM? It is loaded in the background (served from the disk meanwhile) and needs about as much free RAM as the games on it; not enough RAM → it stays on the disk (see the log).':null);
const saveGamesUpdate=name=>gamesCall('/api/games/save',{name},'Save the update of '+name+' as its new version? Its update machine must be shut down. Each machine gets it at its next boot.');
const discardGamesUpdate=name=>gamesCall('/api/games/discard',{name},'Throw the update of '+name+' away?');
const delGames=name=>gamesCall('/api/games/delete',{name},'Delete games disk '+name+' and EVERY game on it? Machines using it keep it until they reboot.');
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
    esc(d.name)+(d.all?all:''),d.all,d.all?'for every machine — change it on the Drivers page':'')).join('')||'—';}
function closeGroup(){grCur=null;const e=document.getElementById('gr_edit');if(e)e.style.display='none';}
async function saveGroup(){const m=document.getElementById('gr_emsg');
  const ids=(id,num)=>[...document.querySelectorAll('#'+id+' input:checked:not(:disabled)')].map(e=>num?+e.dataset.v:e.dataset.v);
  const body={name:grCur,machines:ids('gr_machines',1),images:ids('gr_images',1),disks:ids('gr_disks',0),drivers:ids('gr_drivers',1)};
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
async function deleteGroup(n){if(await sure('Delete group '+n+'? Its machines get no group; it leaves every image, games disk and driver package.'))grCall('/api/groups/delete',{name:n});}

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
  try{await j('/api/password',mk({current:g('pw_cur'),new:g('pw_new')}));say(m,' ✓ changed, other sessions signed out');
    for(const id of ['pw_cur','pw_new','pw_new2'])document.getElementById(id).value='';}
  catch(e){say(m,' ✗ '+e.message);}}
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

// ---- remote (vnc.rs): watch + control a Windows machine over a WebSocket ----
let rmWs=null, rmCtx=null, rmW=0, rmH=0;
async function loadRemote(){
  try{const hosts=await j('/api/vnc/agents');
    document.getElementById('rm_hosts').innerHTML=hosts.map(h=>`<option value="${esc(h)}">`).join('');}catch(e){}
  const q=new URLSearchParams(location.search).get('host');
  if(q){document.getElementById('rm_host').value=q;rmConnect();}
}
// Machines page: open the Remote page for this host.
function remoteTo(host){history.pushState(null,'','/remote?host='+encodeURIComponent(host||''));showPage('remote');}
function rmSay(m){say(document.getElementById('rm_msg'),m);}
function rmClose(){if(rmWs){rmWs.onclose=null;rmWs.close();rmWs=null;}rmCtx=null;
  const c=document.getElementById('rm_canvas');if(c)c.classList.add('d-none');rmSay(' disconnected');}
function rmConnect(){
  rmClose();
  const host=document.getElementById('rm_host').value.trim();if(!host){rmSay(' ✗ enter a hostname');return;}
  const q=+document.getElementById('rm_q').value;
  const proto=location.protocol==='https:'?'wss:':'ws:';
  const ws=new WebSocket(`${proto}//${location.host}/api/vnc/view?host=${encodeURIComponent(host)}&q=${q}`);
  ws.binaryType='arraybuffer';rmWs=ws;rmSay(' connecting…');
  const canvas=document.getElementById('rm_canvas');
  ws.onopen=()=>{rmSay(' connected');canvas.classList.remove('d-none');canvas.focus();};
  ws.onclose=()=>{if(rmWs===ws){rmWs=null;rmSay(' ✗ connection closed (agent gone or another viewer took over)');canvas.classList.add('d-none');}};
  ws.onerror=()=>rmSay(' ✗ could not connect (no agent for that host?)');
  ws.onmessage=ev=>{
    const b=new Uint8Array(ev.data);
    if(b[0]===2){rmW=b[1]|b[2]<<8;rmH=b[3]|b[4]<<8;canvas.width=rmW;canvas.height=rmH;rmCtx=canvas.getContext('2d');return;}
    if(b[0]===1&&rmCtx){
      const x=b[1]|b[2]<<8,y=b[3]|b[4]<<8;
      createImageBitmap(new Blob([b.subarray(9)],{type:'image/jpeg'})).then(bm=>{
        if(rmCtx)rmCtx.drawImage(bm,x,y);bm.close();
        if(rmWs===ws&&ws.readyState===1)ws.send('{"op":"ack"}');
      }).catch(()=>{if(rmWs===ws&&ws.readyState===1)ws.send('{"op":"ack"}');});
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
  loadBenchHist();loadIscsi();if(lastRes)showRes(lastRes);
}
async function loadBenchHist(){
  const sel=document.getElementById('b_hmac');if(!sel)return;
  const rows=await j('/api/bench/history?mac='+encodeURIComponent(sel.value));
  document.querySelector('#b_hist tbody').innerHTML=rows.map(r=>`<tr><td class="mono">${esc(bTime(r.at))}</td><td>${esc(bName(r.mac))}</td>
    <td>${esc(BENCH_KINDS[r.kind]?BENCH_KINDS[r.kind][0]:r.kind)}</td><td><b>${esc(bVal(r))}</b></td><td class="mono">${esc(r.detail)}</td></tr>`).join('')
    ||'<tr><td colspan=5 class="mono">no results yet</td></tr>';
}
// iSCSI now: pushed every second over SSE while the Benchmark page is open (showPage closes it on leaving).
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
    if(d.alerts.length)bar.innerHTML=`<b>${ic('warn')} Server at its limit:</b> ${esc(d.alerts.join(', '))} (threshold ${d.limit}%) — clients boot and load slower. <a class="alert-link" href="/benchmark" data-click="go" data-args='["benchmark"]'>Details</a>`;}
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
  </tbody></table>`;}
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
  wake,manage,regMachine,loadStatus,clearMFilters,mPrev,mNext,setKey,rearmKey,
  loadDevices,dvOpen,dvSave,dvDelete,dvClose,dvBulk,dvRegister,dvImport,dvAdd,dvRender,
  loadImages,addImage,setDefault,toggleCache,toggleBase,toggleSsd,togglePreload,editImgGroups,republish,showVersions,exportImage,editBoot,delImage,
  snapshotImage,rollbackImage,versionToImage,deleteVersion,
  loadGames,saveGames,setGamesUpdate,toggleGamesRam,saveGamesUpdate,discardGamesUpdate,editGames,delGames,
  loadDrivers,uploadDriver,saveDrvTargets,delDriver,
  loadGroups,editGroup,closeGroup,saveGroup,createGroup,renameGroup,deleteGroup,
  loadBench,loadBenchHist,benchServer,benchRequest:()=>benchAsk(false),benchCancel:()=>benchAsk(true),bSelAll,
  applyDhcp,savePw,saveCafe,setTimeout_,setZramReserve,setAlert,setSsd,gsMode,setGoldenStreams,logout,
  remoteTo,rmConnect,rmClose,rmQuality,rmFull,rmSas,rmWin,rmAltTab,rmPaste,
};

let started=false;
function startApp(){
  if(started)return; started=true;
  const srvEvents=new EventSource('/api/events');
  srvEvents.addEventListener('ping',alive);
  srvEvents.addEventListener('job',e=>{alive();const d=JSON.parse(e.data);showJob(d.name,d.status);});
  srvEvents.addEventListener('res',e=>{alive();showRes(JSON.parse(e.data));});
  srvEvents.onerror=()=>setServer(false);
  // (Re)connected: job events sent while the link was down are lost → ask for the status of every followed job.
  srvEvents.onopen=()=>{for(const n in jobWatch)j('/api/images/job?name='+encodeURIComponent(n)).then(r=>showJob(n,r.status)).catch(()=>{});};
  setInterval(()=>{if(lastPing&&Date.now()-lastPing>12000)setServer(false);},3000);
  if(location.pathname!=='/'+pageFromPath())history.replaceState(null,'','/'+pageFromPath());
  showPage(pageFromPath());
  setInterval(()=>{if(curPage==='machines'&&!document.hidden)loadStatus();},15000);
}
startApp();
