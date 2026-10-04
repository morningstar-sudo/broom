async function j(url,opt){const r=await fetch(url,opt);
  if(r.status===401){location.replace('/login');throw new Error('login required');}
  if(!r.ok)throw new Error(await r.text());return r.json()}
const esc=s=>(s==null?'':String(s)).replace(/[&<>"'`]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;','`':'&#96;'}[c]));

// ---- tabs: load HTML fragments on demand (/ui/<page>) + only load data for the tab being viewed ----
const loaders={machines:loadStatus,devices:loadDevices,images:loadImages,drivers:loadDrivers,network:loadDhcp,system:loadCafe};
const fragCache={};
let curPage=null;
async function showPage(p){
  document.querySelectorAll('nav a').forEach(x=>x.classList.toggle('active',x.dataset.page===p));
  if(fragCache[p]===undefined){
    try{fragCache[p]=await (await fetch('/ui/'+p)).text();}
    catch(e){fragCache[p]='<p class="hint">failed to load page '+p+'</p>';}
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
       <td>${m.hostname?esc(m.hostname):'<span class="mono">—</span>'} ${m.registered?'':'<span class="pill new">new</span>'}</td>
       <td class="mono">${m.grp?esc(m.grp):'—'}</td>
       <td class="mono">${esc(m.ip)}</td><td class="mono">${esc(m.mac)}</td>
       <td><span class="led ${m.online?'on':'off'}">${m.online?'ON':'OFF'}</span>${m.not_reset?' <span class="pill" style="background:var(--off,#c33);color:#fff" title="Windows started from the SSD without a PXE boot at '+new Date(m.not_reset*1000).toLocaleString()+' — this session was NOT reset (cable out, server down, boot order changed?). Cleared by the next PXE boot.">not reset</span>':''}</td>
       <td class="mono" title="${esc(m.license_result||'')}">${licCell(m)}</td>
       <td class="row">${m.registered
          ? `<button class="ghost" onclick="wake('${esc(m.mac)}')">Wake</button>
             <button class="ghost" onclick="manage(${m.id})">Manage</button>`
          : `<button onclick="regMachine('${esc(m.mac)}','${esc(m.ip)}','${esc(m.hostname)}')">Register</button>`}</td>
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
  do{hostname=prompt('Hostname for '+mac+(hostOk(hostname)?'':'\n'+HOST_RULE)+':',hostname);if(hostname===null)return;hostname=hostname.trim();}
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
  const badge=ok?' <span class="ok">✓</span>':licErr(m)?' <span class="pill new">error</span>':'';
  if(!full)return `${esc(m.license_state||'')} …${esc(m.license_tail)}${badge}`;
  const res=m.license_result?`<div class="mono" style="white-space:pre-wrap;color:var(--muted);margin-top:4px">${esc(m.license_result)}</div>`:'';
  return `${esc(m.license_state||'')} ${esc(m.license_key||('…'+m.license_tail))}${badge}${res}`;}
async function setKey(id,who){
  let k='';
  do{k=prompt('Windows license key for '+who+(k===''||keyOk(k)?'':'\n'+KEY_RULE)+':',k);if(k===null)return;k=k.trim().toUpperCase();}
  while(k&&!keyOk(k));
  try{await j('/api/machines/license',mk({id,key:k}));
    toast(k?'Key saved. It is installed when '+who+' boots next (its base is rebuilt once).':'Key removed.');refreshMachines();}
  catch(e){toast(e.message,true)}}
async function rearmKey(id,who){if(!confirm('Hand the key to '+who+' once more? Its base is rebuilt on the next boot.'))return;
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
  tb.innerHTML=rows.map(m=>`<tr class="${dvOpenId&&m.id===dvOpenId?'sel':''}" ${m.registered?`onclick="dvOpen(${m.id})" style="cursor:pointer"`:''}>
      <td onclick="event.stopPropagation()"><input type="checkbox" ${dvSel.has(dvKey(m))?'checked':''} onchange="dvToggle('${esc(dvKey(m))}',this.checked)"></td>
      <td>${m.hostname?esc(m.hostname):'<span class="mono">—</span>'} ${m.registered?'':'<span class="pill new">new</span>'}</td>
      <td class="mono">${esc(m.grp||'—')}</td><td class="mono">${esc(m.ip)}</td><td class="mono">${esc(m.mac)}</td>
      <td><span class="led ${m.online?'on':'off'}">${m.online?'ON':'OFF'}</span></td>
      <td class="mono">${esc(imgName(m.image_id)||'—')}</td>
      <td class="mono" title="${esc(m.license_result||'')}">${licCell(m)}</td>
      <td style="white-space:normal">${esc(m.notes||'')}</td></tr>`).join('')
    ||'<tr><td colspan=9 class="mono">no matching machines</td></tr>';
  document.getElementById('dv_sel').textContent=dvSel.size+' selected';
  const all=document.getElementById('dv_all'); if(all)all.checked=rows.length>0&&rows.every(m=>dvSel.has(dvKey(m)));
}
function dvToggle(k,on){on?dvSel.add(k):dvSel.delete(k);document.getElementById('dv_sel').textContent=dvSel.size+' selected';}
function dvSelectAll(on){dvFiltered().forEach(m=>on?dvSel.add(dvKey(m)):dvSel.delete(dvKey(m)));dvRender();}
function dvMsg(t){const e=document.getElementById('dv_msg');if(e)e.textContent=t;}
const dvPicked=()=>dvAll.filter(m=>dvSel.has(dvKey(m)));
async function dvBulk(action){
  const reg=dvPicked().filter(m=>m.registered);
  if(!reg.length){toast('Select registered machines first',true);return;}
  let value='';
  if(action==='group'){value=v('dv_bulk_grp');if(value&&!/^[A-Za-z0-9_-]{1,32}$/.test(value)){toast('Group: letters/digits/_/-, max 32',true);return;}}
  if(action==='image')value=document.getElementById('dv_bulk_img').value;
  if(action==='delete'&&!confirm('Delete '+reg.length+' machine(s)? Their DHCP binding and license key go too.'))return;
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
  const who=esc(m.hostname||m.mac), st=dvAll.find(x=>x.id===id)||{};
  const opt=dvImages.map(i=>`<option value="${i.id}" ${i.id===m.image_id?'selected':''}>${esc(i.name)}</option>`).join('');
  const lease=d.lease?esc(d.lease.ip||'—')+' · '+esc(d.lease.source)+' · '+(d.lease.expires_in>0?'expires in '+Math.round(d.lease.expires_in/60)+' min':'expired'):'—';
  box.hidden=false;
  box.innerHTML=`<h2>${who} &nbsp;<span class="led ${st.online?'on':'off'}">${st.online?'ON':'OFF'}</span></h2>
    <div class="row">
      <div class="field"><label>Hostname</label><input id="dd_host" value="${esc(m.hostname||'')}" size="12" maxlength="15"></div>
      <div class="field"><label>MAC</label><input id="dd_mac" value="${esc(m.mac)}" size="17"></div>
      <div class="field"><label>Fixed IP</label><input id="dd_ip" value="${esc(m.ip||'')}" size="12" placeholder="(none)"></div>
      <div class="field"><label>Group</label><input id="dd_grp" value="${esc(m.grp||'')}" size="8"></div>
      <div class="field"><label>Default image</label><select id="dd_img"><option value="">(global default)</option>${opt}</select></div>
      <div class="field" style="flex:1;min-width:180px"><label>Notes</label><input id="dd_notes" value="${esc(m.notes||'')}" maxlength="200"></div>
    </div>
    <div class="row" style="margin-top:10px">
      <button class="primary" onclick="dvSave(${id})">Save</button>
      <button class="ghost" onclick="wake('${esc(m.mac)}')">Wake</button>
      <button class="ghost" onclick="setKey(${id},'${who}')">License key</button>
      ${m.license_state==='sent'?`<button class="ghost" onclick="rearmKey(${id},'${who}')">Re-arm key</button>`:''}
      <button class="ghost danger" onclick="dvDelete(${id},'${who}')">Delete</button>
      <button class="ghost" onclick="dvClose()">Close</button>
      <span id="dd_msg" class="msg"></span>
    </div>
    <table style="margin-top:12px"><tbody>
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
  if(body.hostname&&!hostOk(body.hostname)){msg.textContent=' ✗ '+HOST_RULE;return;}
  try{await j('/api/machines/update',mk(body));msg.textContent=' ✓ saved';loadDevices();}
  catch(e){msg.textContent=' ✗ '+e.message;}
}
async function dvDelete(id,who){if(!confirm('Delete '+who+'? Its DHCP binding and license key go too.'))return;
  try{await j('/api/machines/delete',mk({ids:[id]}));dvClose();loadDevices();}catch(e){toast(e.message,true);}}

// ---- images ----
const gbs=b=>b>=1e9?(b/1e9).toFixed(1)+' GB':(b/1e6).toFixed(0)+' MB';
let imgRows=[];
async function loadImages(){
  const rows=imgRows=await j('/api/images');
  document.querySelector('#images tbody').innerHTML=rows.map(i=>
    `<tr><td>${esc(i.name)}${i.export?`<div class="mono" style="font-size:12px" title="exported ${i.export.created?new Date(i.export.created*1000).toLocaleString():''} — save both into one folder, open the .vmx in VMware">⬇ <a href="/api/images/export-file?id=${i.id}&f=vmx">.vmx</a> · <a href="/api/images/export-file?id=${i.id}&f=vmdk">.vmdk ${gbs(i.export.size)}</a> (${esc(i.export.version)})</div>`:''}</td><td><span class="pill ${i.os}">${esc(i.os)}</span></td>
       <td class="mono">${esc(i.cache_mode||'disk')}</td>
       <td>${i.is_default?'<span class="ok">✓</span>':''}</td>
       <td>${i.boot_script?'<span class="ok">✓</span>':'<span class="mono">no</span>'}</td>
       <td class="mono" title="used on disk / virtual disk">${i.size==null?'—':gbs(i.used)+' / '+gbs(i.size)}${i.active_version?' <span class="pill linux">'+esc(i.active_version)+'</span>':''}</td>
       <td class="mono" title="${esc(i.hash||'')}">${i.hash?esc(i.hash).slice(0,10):'—'}</td>
       <td class="row">
         <button class="ghost" onclick="setDefault(${i.id})">Default</button>
         <button class="ghost" onclick="toggleCache(${i.id},'${esc(i.cache_mode||'disk')}','${esc(i.name)}')">${(i.cache_mode==='zram')?'→disk':'→zram'}</button>
         ${i.os==='windows'?`<button class="ghost" onclick="toggleBase(${i.id},${!i.base_mode})" title="BASE MODE: the first logon on each machine waits for a technician to set up apps, then restart (saved for every boot). Off: base is saved by itself.">${i.base_mode?'Base mode: ON':'Base mode: off'}</button>`:`<button class="ghost" onclick="toggleSsd(${i.id},${!i.use_ssd})" title="On: the golden is cached and the session's writes go to the machine's SSD (reset every boot). Off (one-time): nothing touches the SSD — golden over the network, writes in RAM, gone at power-off.">${i.use_ssd?'SSD: on':'SSD: off (one-time)'}</button>`}
         <button class="ghost" onclick="republish(${i.id},'${esc(i.name)}')">Republish</button>
         <button class="ghost" onclick="showVersions(${i.id},'${esc(i.name)}','${esc(i.os)}')">Versions</button>
         <button class="ghost" onclick="exportImage(${i.id},'${esc(i.name)}',null)" title="download as a VMware VM (.vmx + .vmdk) to edit the golden">Export</button>
         <button class="ghost" onclick="editBoot(${i.id})">Boot</button>
         <button class="ghost danger" onclick="delImage(${i.id},'${esc(i.name)}')">Delete</button>
       </td></tr>`).join('') || '<tr><td colspan=8 class="mono">no images yet</td></tr>';
  // srvhost inside the images fragment → set after it is injected.
  try{document.getElementById('srvhost2').textContent=location.host;}catch(_){}
}
async function toggleCache(id,cur,name){const mode=cur==='zram'?'disk':'zram';
  if(!confirm('Switch image cache to "'+mode+'"? (republish; zram loads the img into RAM)'))return;
  const el=document.getElementById('img_status');
  try{await j('/api/images/cache-mode',mk({id,mode}));watchJob(name,el);}catch(e){el.textContent=' ✗ '+e.message;}}
async function toggleBase(id,on){
  if(on&&!confirm('BASE MODE: every machine that builds its base (first boot, new golden, rename, drivers) will wait on the desktop until someone restarts it — whatever is done before that restart is kept for good. Turn on?'))return;
  const el=document.getElementById('img_status');
  try{await j('/api/images/base-mode',mk({id,on}));loadImages();}catch(e){el.textContent=' ✗ '+e.message;}}
async function toggleSsd(id,on){
  const el=document.getElementById('img_status');
  try{await j('/api/images/ssd',mk({id,on}));loadImages();}catch(e){el.textContent=' ✗ '+e.message;}}
// One-time link for the Windows prep script (it carries the guest password): valid once, for an hour.
async function prepCmd(){const el=document.getElementById('prep_cmd');
  try{const r=await j('/api/prep-token',{method:'POST'});
    el.textContent='irm "http://'+location.host+'/broom-prep-win?t='+r.token+'" | iex';}
  catch(e){el.textContent='✗ '+e.message;}}
async function setDefault(id){await j('/api/images/default',mk({id}));loadImages()}
// Job status by image name, pushed over SSE (/api/events "job") → text of el until ✓/✗.
// done(): optional, called when the job finishes (e.g. refresh the versions list).
const jobWatch={};
function showJob(name,s){const w=jobWatch[name];if(!w||!s)return;
  w.el.textContent=' '+s;
  if(s.startsWith('✓')||s.startsWith('✗')){delete jobWatch[name];loadImages();if(w.done)w.done();}}
function watchJob(name,el,done){
  el.textContent=' ⏳ working...';jobWatch[name]={el,done};
  // catch up once: the job may have moved on (or finished) before this tab started watching
  j('/api/images/job?name='+encodeURIComponent(name)).then(r=>showJob(name,r.status)).catch(()=>{});
}
// ---- image versions (versions.rs: dedup 4 MB chunks; snapshot/rollback run as jobs) ----
let verImg=null;
async function showVersions(id,name,os){verImg={id,name,os};
  const el=document.getElementById('img_versions');
  el.innerHTML=`<h2>Versions — ${esc(name)}</h2><p class="mono">⏳ comparing with the current golden (the first time after a change reads the whole golden)...</p>`;
  const r=await j('/api/images/snapshots?id='+id);
  el.innerHTML=`<h2>Versions — ${esc(name)} <button class="ghost" onclick="snapshotImage()">+ Snapshot now</button></h2>
    <div class="scroll"><table><thead><tr><th>Version</th><th>Label</th><th>Created</th><th title="data that differs from the golden being served now (image list) — what a rollback to this version rewrites">vs current golden</th><th></th></tr></thead><tbody>${
    r.versions.map(v=>`<tr><td class="mono">${esc(v.version)} ${v.version===r.active?'<span class="pill linux">active</span>':''}</td>
      <td>${esc(v.label)}</td><td class="mono">${new Date(v.created*1000).toLocaleString()}</td><td class="mono" title="disk size ${gbs(v.size)}">${v.diff==null?'<span title="no golden on the server">?</span>':v.diff?'Δ '+gbs(v.diff):'same'}</td>
      <td class="row"><button class="ghost" onclick="rollbackImage('${esc(v.version)}')">Rollback</button>
        <button class="ghost" onclick="exportImage(verImg.id,verImg.name,'${esc(v.version)}')">Export</button>
        <button class="ghost" onclick="versionToImage('${esc(v.version)}')" title="add this version as a new image on the list">→ New image</button>
        <button class="ghost danger" onclick="deleteVersion('${esc(v.version)}')">Delete</button></td></tr>`).join('')
    || '<tr><td colspan=5 class="mono">no versions yet — Snapshot now saves the current golden</td></tr>'}</tbody></table></div>`;}
async function snapshotImage(){const label=prompt('Label for this version (optional):','');if(label===null)return;
  const el=document.getElementById('img_status');
  try{await j('/api/images/snapshot',mk({id:verImg.id,label}));watchJob(verImg.name,el,()=>showVersions(verImg.id,verImg.name,verImg.os));}catch(e){el.textContent=' ✗ '+e.message;}}
async function rollbackImage(version){
  if(!confirm('Roll '+verImg.name+' back to '+version+'?'+(verImg.os==='windows'?' Every client will download the golden again.':' Running clients lose their iSCSI disk until publish finishes.')))return;
  const el=document.getElementById('img_status');
  try{await j('/api/images/rollback',mk({id:verImg.id,version}));watchJob(verImg.name,el,()=>showVersions(verImg.id,verImg.name,verImg.os));}catch(e){el.textContent=' ✗ '+e.message;}}
async function deleteVersion(version){if(!confirm('Delete version '+version+' of '+verImg.name+'?'))return;
  try{await j('/api/images/version-delete',mk({id:verImg.id,version}));}catch(e){toast(e.message,true);}
  showVersions(verImg.id,verImg.name,verImg.os);}
async function exportImage(id,name,version){
  if(!confirm('Export '+name+(version?' '+version:' (current golden)')+' as a VMware VM (.vmx + .vmdk)?\nNeeds free disk space on the server ≈ the golden size; replaces the previous export of this image.'))return;
  const el=document.getElementById('img_status');
  try{await j('/api/images/export',mk({id,version}));watchJob(name,el);}catch(e){el.textContent=' ✗ '+e.message;}}
async function versionToImage(version){
  const name=prompt('New image name for '+verImg.name+' '+version+' (letters, digits, _ -):',verImg.name+'-'+version);
  if(!name)return;
  const el=document.getElementById('img_status');
  try{await j('/api/images/from-version',mk({id:verImg.id,version,name}));loadImages();watchJob(name,el);}catch(e){el.textContent=' ✗ '+e.message;}}
async function republish(id,name){const el=document.getElementById('img_status');
  try{await j('/api/images/publish',mk({id}));watchJob(name,el);}catch(e){el.textContent=' ✗ '+e.message;}}
async function editBoot(id){const cur=(imgRows.find(i=>i.id===id)||{}).boot_script||'';const s=prompt('iPXE boot script:',cur);if(s===null)return;
  await j('/api/images/boot-script',mk({id,boot_script:s}));loadImages();}
async function delImage(id,name){if(!confirm('Delete image "'+name+'"?'))return;
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
  prog.style.display='';prog.value=0;
  const lane=async()=>{while(q.length){const [f,o]=q.shift();
    try{await putChunk(api,name,f,o);}catch(e){q.length=0;throw e;}   // one piece failed for good → stop all lanes
    done+=Math.min(CHUNK,f.size-o);prog.value=done/total*100;
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
  catch(e){msg.textContent=' ✗ '+e.message;return;}
  msg.textContent=' ✓ upload done';loadImages();watchJob(name,msg);   // convert + publish run in the background → SSE
}

// ---- driver packages (drivers.rs): .zip of an extracted driver folder → installed into base by broom-done ----
async function loadDrivers(){
  const rows=await j('/api/drivers');
  document.querySelector('#drivers tbody').innerHTML=rows.map(d=>
    `<tr><td>${esc(d.name)}</td><td class="mono">${gbs(d.size)}</td>
       <td class="mono" title="${esc(d.hwids.slice(0,40).join('\n'))}${d.hwids.length>40?'\n…':''}">${d.hwids.length}</td>
       <td class="mono" style="white-space:normal">${d.machines.length?esc(d.machines.join(', ')):'—'}</td>
       <td><input type="checkbox" id="drv_all_${d.id}" ${d.all_machines?'checked':''}></td>
       <td><input id="drv_grp_${d.id}" value="${esc(d.groups.join(', '))}" placeholder="VIP, Pro" size="12"></td>
       <td class="row"><button class="ghost" onclick="saveDrvTargets(${d.id})">Save</button>
         <button class="ghost danger" onclick="delDriver(${d.id},'${esc(d.name)}')">Delete</button></td></tr>`).join('')
    || '<tr><td colspan=7 class="mono">no driver packages yet</td></tr>';
}
async function uploadDriver(){
  const f=document.getElementById('drv_file').files[0],msg=document.getElementById('drv_msg');
  const name=v('drv_name')||(f?f.name.replace(/\.zip$/i,'').replace(/[^A-Za-z0-9_-]/g,'-'):'');
  if(!f){toast('Choose the .zip of an extracted driver folder',true);return;}
  try{const r=await uploadFiles('/api/drivers',name,[f],document.getElementById('drv_prog'),msg);
    msg.textContent=' ✓ '+name+': '+r.infs+' .inf, '+r.hwids+' hardware IDs';loadDrivers();}
  catch(e){msg.textContent=' ✗ '+e.message;}
}
async function saveDrvTargets(id){
  const all_machines=document.getElementById('drv_all_'+id).checked, groups=document.getElementById('drv_grp_'+id).value;
  try{await j('/api/drivers/targets',mk({id,all_machines,groups}));loadDrivers();}catch(e){toast(e.message,true)}}
async function delDriver(id,name){if(!confirm('Delete driver package "'+name+'"? Machines that had it rebuild their base on the next boot.'))return;
  try{await j('/api/drivers/delete',mk({id}));loadDrivers();}catch(e){toast(e.message,true)}}

// ---- network ----
// DNS servers: one box each (order = preference), up to 8; sent to the server as one comma-separated list.
const DNS_MAX=8;
function dnsBox(v){const w=document.createElement('span');w.style.cssText='display:inline-flex;gap:2px';
  w.innerHTML='<input class="dns" size="14" placeholder="e.g. 1.1.1.1"><button type="button" class="ghost" title="remove">×</button>';
  w.querySelector('input').value=v||'';w.querySelector('button').onclick=()=>{w.remove();dnsButton();};return w;}
function dnsButton(){const l=document.getElementById('dns_list');if(!l)return;let b=document.getElementById('dns_add');
  if(!b){b=document.createElement('button');b.id='dns_add';b.type='button';b.className='ghost';b.textContent='+ DNS';
    b.onclick=()=>{l.insertBefore(dnsBox(''),b);dnsButton();};l.appendChild(b);}
  b.style.display=l.querySelectorAll('input.dns').length>=DNS_MAX?'none':'';}
function setDns(csv){const l=document.getElementById('dns_list');if(!l)return;l.innerHTML='';
  const v=(csv||'').split(',').map(s=>s.trim()).filter(Boolean);for(const x of (v.length?v:[''])) l.appendChild(dnsBox(x));dnsButton();}
function getDns(){return [...document.querySelectorAll('#dns_list input.dns')].map(e=>e.value.trim()).filter(Boolean).join(',');}
async function loadDhcp(){const c=await j('/api/dhcp');
  ['mode','iface','server_ip','subnet','range_start','range_end','gateway'].forEach(k=>{const e=document.getElementById('dhcp_'+k);if(e)e.value=c[k]||'';});
  for(const k of ['ipxe_signed','strict_reset','rapid_commit','ipxe_fast','authoritative','send_hostname']){const e=document.getElementById('dhcp_'+k);if(e)e.checked=!!c[k];}
  setDns(c.dns);
  const ps=document.getElementById('pxe_srv');if(ps&&c.server_ip)ps.textContent=c.server_ip;
  const dm=document.getElementById('dhcp_mode');if(dm)dm.onchange=toggleFull;toggleFull();}
function toggleFull(){const dm=document.getElementById('dhcp_mode');if(!dm)return;
  for(const id of ['full_only','full_only_opts']){const e=document.getElementById(id);if(e)e.style.display=dm.value==='full'?'':'none';}}
async function applyDhcp(){const b={};['mode','iface','server_ip','subnet','range_start','range_end','gateway'].forEach(k=>b[k]=document.getElementById('dhcp_'+k).value);
  for(const k of ['ipxe_signed','strict_reset','rapid_commit','ipxe_fast','authoritative','send_hostname']){const e=document.getElementById('dhcp_'+k);if(e)b[k]=e.checked?'1':'0';}
  b.dns=getDns();
  const m=document.getElementById('dhcp_msg');
  try{const r=await j('/api/dhcp',mk(b));m.textContent=' ✓ '+(r.status||'applied');}catch(e){m.textContent=' ✗ '+e.message;}}

// ---- system ----
async function loadCafe(){
  j('/api/config').then(c=>{const t=document.getElementById('timeout'),z=document.getElementById('zram_reserve');
    if(t)t.value=c.boot_timeout;if(z)z.value=c.zram_reserve_mb;}).catch(()=>{});
  const c=await j('/api/cafe-user');
  const u=document.getElementById('cafe_user');if(u)u.value=c.user||'';
  const p=document.getElementById('cafe_password');if(p)p.placeholder=c.password_set?'(unchanged)':'set a password';}
async function saveCafe(){const m=document.getElementById('cafe_msg');
  // Password is write-only: send it only when the field is filled (blank = keep the stored one).
  const body={user:v('cafe_user')},pw=document.getElementById('cafe_password').value;if(pw)body.password=pw;
  try{await j('/api/cafe-user',mk(body));m.textContent=' ✓ saved';document.getElementById('cafe_password').value='';loadCafe();}
  catch(e){m.textContent=' ✗ '+e.message;}}
async function savePw(){const m=document.getElementById('pw_msg'),g=id=>document.getElementById(id).value;
  if(g('pw_new').length<8){m.textContent=' ✗ at least 8 characters';return;}
  if(g('pw_new')!==g('pw_new2')){m.textContent=' ✗ passwords do not match';return;}
  try{await j('/api/password',mk({current:g('pw_cur'),new:g('pw_new')}));m.textContent=' ✓ changed, other sessions signed out';
    for(const id of ['pw_cur','pw_new','pw_new2'])document.getElementById(id).value='';}
  catch(e){m.textContent=' ✗ '+e.message;}}
async function setTimeout_(){await j('/api/config/timeout',mk({seconds:+document.getElementById('timeout').value}));document.getElementById('to_msg').textContent=' ✓ saved';}
async function setZramReserve(){await j('/api/config/zram-reserve',mk({mb:+document.getElementById('zram_reserve').value}));document.getElementById('zr_msg').textContent=' ✓ saved';}

// ---- server liveness (SSE /api/events: "ping" on connect + every 5 s) ----
// Green on ping; red on connection error or no ping for 12 s (e.g. server powered off, half-open link).
// EventSource reconnects by itself → green again on the next ping.
let lastPing=0;
function setServer(up){const d=document.getElementById('srvdot');
  d.classList.toggle('up',up);d.classList.toggle('down',!up);d.title=up?'server online':'server offline';}

// ---- helpers ----
function v(id){return document.getElementById(id).value.trim()}
function mk(body){return {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)}}
// Non-blocking notification. Replaces toast() so errors can't be hidden by "prevent additional dialogs".
let toastT;
function toast(msg,err){let t=document.getElementById('toast');
  if(!t){t=document.createElement('div');t.id='toast';document.body.appendChild(t);}
  t.textContent=msg;t.className='show'+(err?' err':'');
  clearTimeout(toastT);toastT=setTimeout(()=>{t.className=t.className.replace('show','').trim();},err?6000:3500);}

// ---- admin login (auth.rs) ----
// Login lives on its own page (/login). This page is only served to a signed-in admin: the server redirects an
// unauthenticated browser here → /login. A 401 on any API call (session expired) sends us back to /login.
async function logout(){try{await fetch('/api/auth/logout',{method:'POST'});}catch(e){}location.replace('/login');}

let started=false;
function startApp(){
  if(started)return; started=true;
  const srvEvents=new EventSource('/api/events');
  srvEvents.addEventListener('ping',()=>{lastPing=Date.now();setServer(true);});
  srvEvents.addEventListener('job',e=>{const d=JSON.parse(e.data);showJob(d.name,d.status);});
  srvEvents.onerror=()=>setServer(false);
  setInterval(()=>{if(lastPing&&Date.now()-lastPing>12000)setServer(false);},3000);
  if(location.pathname!=='/'+pageFromPath())history.replaceState(null,'','/'+pageFromPath());
  showPage(pageFromPath());
  setInterval(()=>{if(curPage==='machines')loadStatus();},15000);
}
startApp();
