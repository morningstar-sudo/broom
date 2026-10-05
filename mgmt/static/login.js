let setup=false;
const $=id=>document.getElementById(id);
// First run (no admin password yet) → switch to "set a password" mode. Also skip the page if already signed in.
(async()=>{
  try{
    const s=await(await fetch('/api/auth/status')).json();
    if(s.authed){location.replace('/');return;}
    if(!s.configured){
      setup=true;
      $('title').textContent='Set the admin password';
      $('hint').textContent='First run — enter the setup token printed in the server log (journalctl -u bootrom-mgmt, or the terminal running it), then choose the admin password (min 8 characters).';
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
    {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(setup?{password:pw,token:$('tok').value.trim()}:{password:pw})});}
  catch(err){msg.textContent='✗ '+err.message;$('go').disabled=false;return;}
  if(!r.ok){msg.textContent='✗ '+((await r.text())||('HTTP '+r.status));$('go').disabled=false;return;}
  location.replace('/');   // session cookie set → the app is now reachable
};
