let setup=false;
const $=id=>document.getElementById(id);
// Bootstrap colour mode follows the OS (same as app.js).
const darkQ=matchMedia('(prefers-color-scheme: dark)');
const setTheme=()=>document.documentElement.dataset.bsTheme=darkQ.matches?'dark':'light';
setTheme();darkQ.addEventListener('change',setTheme);
// Error under the form: the error icon (/icons.svg) + the text as a text node (never parsed as HTML).
function fail(t){const m=$('msg');m.innerHTML='<svg class="ic" role="img" aria-label="error"><use href="/icons.svg#err"/></svg> ';m.append(t);$('go').disabled=false;}
// First run (no admin password yet) → switch to "set a password" mode. Also skip the page if already signed in.
(async()=>{
  try{
    const s=await(await fetch('/api/auth/status')).json();
    if(s.authed){location.replace('/');return;}
    if(!s.configured){
      setup=true;
      $('title').textContent='Create the owner account';
      $('hint').textContent='First run — enter the setup token printed in the server log (journalctl -u bootrom-mgmt, or the terminal running it), then choose the owner account name and its password (min 8 characters).';
      $('pw').autocomplete='new-password';$('pw2').hidden=false;$('tok').hidden=false;$('tok').focus();
    }
  }catch(e){/* offline — let the submit surface the error */}
})();
$('f').onsubmit=async e=>{
  e.preventDefault();
  const pw=$('pw').value, msg=$('msg');
  if(setup){
    if(pw.length<8){msg.textContent='at least 8 characters';return;}
    if(pw!==$('pw2').value){msg.textContent='passwords do not match';return;}
    if(!$('tok').value.trim()){msg.textContent='enter the setup token';return;}
  }
  $('go').disabled=true;msg.textContent='';
  let r;
  try{r=await fetch(setup?'/api/auth/setup':'/api/auth/login',
    {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({user:$('user').value.trim().toLowerCase(),password:pw,...(setup?{token:$('tok').value.trim()}:{})})});}
  catch(err){fail(err.message);return;}
  if(!r.ok){fail((await r.text())||('HTTP '+r.status));return;}
  location.replace('/');   // session cookie set → the app is now reachable
};
