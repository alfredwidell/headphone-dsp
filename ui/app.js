/* Headphone DSP — dependency-free, offline studio UI.
 * Native responses are authoritative. Browser mode is a visual simulation only;
 * its analytical crossfeed approximation is not a replacement for KU100 data.
 */
(() => {
  'use strict';
  const $ = id => document.getElementById(id);
  const clone = value => JSON.parse(JSON.stringify(value));
  const clamp = (v, min, max) => Math.min(max, Math.max(min, v));
  const signed = (n, digits = 1) => `${n < 0 ? '−' : '+'}${Math.abs(n).toFixed(digits)}`;
  const colors = ['#7dd3c8', '#e8b34b', '#c59af2', '#e68b9b', '#85a9ee', '#bed583', '#e6a878', '#86c5e5', '#d995d1', '#8fd6ae', '#c6b88e', '#a8a0e8'];
  const types = {bell:'Bell / Peaking', low_shelf:'Low shelf', high_shelf:'High shelf', high_pass:'High pass', low_pass:'Low pass', notch:'Notch'};
  const gainTypes = new Set(['bell', 'low_shelf', 'high_shelf']);
  const previewMode = !window.__TAURI__?.core?.invoke;
  const defaults = {
    capture_backend:'Pulse', capture_device:'headphone_dsp_sink.monitor', capture_loopback:true,
    playback_backend:'Alsa', playback_device:'plughw:CARD=Generic_1,DEV=0', exclusive_mode:false,
    sample_rate:48000, chunksize:2048, master_bypass:false, preamp_db:0, auto_preamp:true,
    crossfeed_enabled:true, crossfeed_angle_deg:35, crossfeed_mid_boost_db:3, eq_enabled:true,
    eq_bands:[{enabled:true,band_type:'low_shelf',freq_hz:105,gain_db:4,q:.71}, {enabled:true,band_type:'bell',freq_hz:1000,gain_db:0,q:1}, {enabled:true,band_type:'high_shelf',freq_hz:8000,gain_db:-1.5,q:.71}], active_profile:'Default'
  };
  let state = null, settings = clone(defaults), revision = 0, selectedBand = -1;
  let updateTimer, queue = Promise.resolve(), pollTimer, pollBusy = false, pollGeneration = 0;
  let ready = false, drag = null, frame = null, toastError = '', clipAcknowledged = 0;
  let mockSettings = clone(defaults), mockRunning = false, mockClips = 0;
  let profileAction = 'save', engineBusy = false, actionDepth = 0;
  const storageKey = 'headphone-dsp.preview.profiles.v1';
  const builtins = {
    'Flat / Neutral': {...clone(defaults), eq_bands:[], crossfeed_enabled:false},
    'Warm Vocal & Sub-Bass': {...clone(defaults), eq_bands:[{enabled:true,band_type:'low_shelf',freq_hz:85,gain_db:4.5,q:.71},{enabled:true,band_type:'bell',freq_hz:1800,gain_db:1.5,q:1.1},{enabled:true,band_type:'high_shelf',freq_hz:7500,gain_db:-2,q:.71}]},
    'HD 650 / 6XX Harman Target': {...clone(defaults), eq_bands:[{enabled:true,band_type:'low_shelf',freq_hz:105,gain_db:5.5,q:.71},{enabled:true,band_type:'bell',freq_hz:3100,gain_db:-2.2,q:1.8},{enabled:true,band_type:'high_shelf',freq_hz:9000,gain_db:2,q:.71}]}
  };
  let mockProfiles = clone(builtins);
  try { const stored = JSON.parse(localStorage.getItem(storageKey) || 'null'); if (stored && typeof stored === 'object' && !Array.isArray(stored)) mockProfiles = {...stored}; } catch { /* Private browser / invalid storage: use defaults. */ }

  // Robert Bristow-Johnson audio EQ cookbook biquads, normalized by a0.
  function coefficients(band, rate) {
    const w = 2 * Math.PI * clamp(band.freq_hz, 1, rate * .499) / rate;
    const c = Math.cos(w), s = Math.sin(w), q = Math.max(.05, band.q);
    const a = 10 ** (band.gain_db / 40), alpha = s / (2 * q), beta = 2 * Math.sqrt(a) * alpha;
    let b0, b1, b2, a0, a1, a2;
    switch (band.band_type) {
      case 'low_shelf':
        b0=a*((a+1)-(a-1)*c+beta); b1=2*a*((a-1)-(a+1)*c); b2=a*((a+1)-(a-1)*c-beta);
        a0=(a+1)+(a-1)*c+beta; a1=-2*((a-1)+(a+1)*c); a2=(a+1)+(a-1)*c-beta; break;
      case 'high_shelf':
        b0=a*((a+1)+(a-1)*c+beta); b1=-2*a*((a-1)+(a+1)*c); b2=a*((a+1)+(a-1)*c-beta);
        a0=(a+1)-(a-1)*c+beta; a1=2*((a-1)-(a+1)*c); a2=(a+1)-(a-1)*c-beta; break;
      case 'high_pass': b0=(1+c)/2; b1=-(1+c); b2=b0; a0=1+alpha; a1=-2*c; a2=1-alpha; break;
      case 'low_pass': b0=(1-c)/2; b1=1-c; b2=b0; a0=1+alpha; a1=-2*c; a2=1-alpha; break;
      case 'notch': b0=1; b1=-2*c; b2=1; a0=1+alpha; a1=-2*c; a2=1-alpha; break;
      default: b0=1+alpha*a; b1=-2*c; b2=1-alpha*a; a0=1+alpha/a; a1=-2*c; a2=1-alpha/a;
    }
    return [b0/a0,b1/a0,b2/a0,a1/a0,a2/a0];
  }
  function responseAt(coeff, frequency, rate) {
    const w = 2*Math.PI*frequency/rate, c = Math.cos(w), s = Math.sin(w), c2 = Math.cos(2*w), s2 = Math.sin(2*w);
    const [b0,b1,b2,a1,a2] = coeff;
    const numerator = (b0+b1*c+b2*c2)**2 + (b1*s+b2*s2)**2;
    const denominator = (1+a1*c+a2*c2)**2 + (a1*s+a2*s2)**2;
    return 10*Math.log10(Math.max(1e-16,numerator)/Math.max(1e-16,denominator));
  }
  function computePEQ(s) {
    const freq_hz = Array.from({length:280}, (_,i) => 20 * 1000**(i/279));
    const per_band_db = s.eq_bands.map(band => {
      const coeff = coefficients(band,s.sample_rate);
      return freq_hz.map(f => band.enabled ? responseAt(coeff,f,s.sample_rate) : 0);
    });
    const composite_db = freq_hz.map((_,i) => s.eq_enabled ? per_band_db.reduce((sum,values) => sum+values[i],0) : 0);
    const raw_peak_db = Math.max(0,...composite_db);
    const effective_preamp_db = s.master_bypass ? 0 : s.preamp_db - (s.auto_preamp ? Math.max(0,raw_peak_db + .5) : 0);
    return {freq_hz,per_band_db,composite_db,raw_peak_db,effective_preamp_db};
  }
  function computeCrossfeed(s) {
    const freq_hz = Array.from({length:110},(_,i)=>20*1000**(i/109));
    const angle = s.crossfeed_angle_deg * Math.PI/180, delay = .00064*Math.sin(angle);
    const mid_db=[], side_db=[], ild_db=[];
    freq_hz.forEach(f => {
      // A simple frequency-dependent opposite-ear transfer with interaural delay.
      // This deliberately approximates, rather than claims to reproduce, KU100 HRTFs.
      const cross = .48 / Math.sqrt(1+(f/(900+900*Math.cos(angle)))**2);
      const phase = 2*Math.PI*f*delay;
      const normalization = 1.48;
      const mid = Math.sqrt(Math.max(1e-12,1+cross*cross+2*cross*Math.cos(phase)))/normalization;
      const side = Math.sqrt(Math.max(1e-12,1+cross*cross-2*cross*Math.cos(phase)))/normalization;
      const boost = s.crossfeed_mid_boost_db*(1-Math.exp(-f/600))*Math.exp(-f/14000);
      mid_db.push(s.crossfeed_enabled ? 20*Math.log10(mid)+boost : 0);
      side_db.push(s.crossfeed_enabled ? 20*Math.log10(side) : 0);
      ild_db.push(s.crossfeed_enabled ? Math.min(24,-20*Math.log10(Math.max(.001,cross))) : 0);
    });
    return {freq_hz,mid_db,side_db,ild_db};
  }
  function mockMeters() {
    const base = mockRunning ? -17+Math.sin(Date.now()/900)*4 : -120;
    const input = [base,base-1.8+Math.sin(Date.now()/740)*1.5];
    const preamp = computePEQ(mockSettings).effective_preamp_db;
    const output = input.map(v=>mockRunning?v+preamp:-120);
    return {running:mockRunning,state:mockRunning?'Running':'Stopped',capture_peak:input,playback_peak:output,capture_rms:input.map(v=>v-8),playback_rms:output.map(v=>v-8),load_percent:mockRunning?2.4+Math.sin(Date.now()/1500)*.5:0,clipped_samples:mockClips,error:null};
  }
  function mockResponse() {
    return {settings:clone(mockSettings),capture_devices:[{id:'headphone_dsp_sink.monitor',label:'[Virtual Sink Loopback] Headphone DSP Virtual Input',backend:'Pulse',is_loopback:true,channels:2},{id:'default',label:'System microphone (not loopback)',backend:'Pulse',is_loopback:false,channels:2}],playback_devices:[{id:'plughw:CARD=Generic_1,DEV=0',label:'ALC257 Analog (ALSA Direct)',backend:'Alsa',is_loopback:false,channels:2},{id:'default',label:'System default output',backend:'Pulse',is_loopback:false,channels:2}],profiles:Object.keys(mockProfiles),peq_preview:computePEQ(mockSettings),crossfeed_preview:computeCrossfeed(mockSettings),meters:mockMeters()};
  }
  function persistProfiles() {
    try { localStorage.setItem(storageKey,JSON.stringify(mockProfiles)); }
    catch { toast('Profile is available for this session only; browser storage is unavailable.'); }
  }
  function parseAutoEQ(text) {
    const bands = []; let preamp = null;
    const codes = {PK:'bell',PEQ:'bell',LS:'low_shelf',LSC:'low_shelf',HS:'high_shelf',HSC:'high_shelf',HP:'high_pass',HPQ:'high_pass',LP:'low_pass',LPQ:'low_pass',NO:'notch',NOTCH:'notch'};
    const number = '([+-]?(?:\\d+(?:\\.\\d*)?|\\.\\d+))';
    for (const raw of text.split(/\r?\n/)) {
      const line = raw.replace(/#.*/, '').trim(); if (!line) continue;
      const p = line.match(new RegExp('^Preamp:\\s*'+number+'\\s*dB\\s*$','i'));
      if (p) { preamp=Number(p[1]); if (preamp < -18 || preamp > 12) throw Error('Preamp must be between −18 and +12 dB.'); continue; }
      if (!/^Filter\s/i.test(line)) continue;
      const head = line.match(/^Filter\s+\d+:\s*(ON|OFF)\s+([A-Z]+)\b/i);
      if (!head || !codes[head[2].toUpperCase()]) throw Error(`Unsupported filter: ${line}`);
      const type = codes[head[2].toUpperCase()];
      const f = line.match(new RegExp('\\bFc\\s+'+number+'\\s*Hz','i'));
      const g = line.match(new RegExp('\\bGain\\s+'+number+'\\s*dB','i'));
      const q = line.match(new RegExp('\\bQ\\s+'+number+'(?:\\s|$)','i'));
      if (!f || (gainTypes.has(type) && !g)) throw Error(`Missing frequency or gain: ${line}`);
      const band = {enabled:head[1].toUpperCase()==='ON',band_type:type,freq_hz:Number(f[1]),gain_db:g?Number(g[1]):0,q:q?Number(q[1]):.7071};
      if (band.freq_hz<20 || band.freq_hz>20000 || Math.abs(band.gain_db)>18 || band.q<.1 || band.q>20) throw Error('Filters must use 20–20,000 Hz, ±18 dB, and Q 0.1–20.');
      bands.push(band);
    }
    if (!bands.length && preamp === null) throw Error('No supported filters or preamp found. Paste EqualizerAPO / AutoEQ text.');
    if (bands.length>12) throw Error('A preset can contain at most 12 filters.');
    return {bands,preamp};
  }
  async function mockInvoke(command,args={}) {
    switch(command) {
      case 'get_live_meters': return mockMeters();
      case 'update_dsp_settings': mockSettings=clone(args.settings); break;
      case 'start_engine': mockSettings=clone(args.settings); mockRunning=true; break;
      case 'stop_engine': mockRunning=false; break;
      case 'save_profile': Object.defineProperty(mockProfiles,args.name,{value:clone(mockSettings),enumerable:true,writable:true,configurable:true}); mockSettings.active_profile=args.name; persistProfiles(); break;
      case 'load_profile': if (!Object.hasOwn(mockProfiles,args.name)) throw Error('Profile not found.'); mockSettings={...mockSettings,...clone(mockProfiles[args.name]),active_profile:args.name}; break;
      case 'delete_profile': delete mockProfiles[args.name]; if (mockSettings.active_profile===args.name) mockSettings.active_profile='Custom'; persistProfiles(); break;
      case 'import_autoeq': {
        const parsed=parseAutoEQ(args.text); mockSettings.eq_bands=parsed.bands; mockSettings.eq_enabled=true;
        if(parsed.preamp!==null) { mockSettings.preamp_db=parsed.preamp; mockSettings.auto_preamp=false; }
        mockSettings.active_profile='Imported AutoEQ'; break;
      }
      case 'export_autoeq': {
        const code={bell:'PK',low_shelf:'LS',high_shelf:'HS',high_pass:'HPQ',low_pass:'LPQ',notch:'NO'};
        return {text:`Preamp: ${computePEQ(mockSettings).effective_preamp_db.toFixed(2)} dB\n`+mockSettings.eq_bands.map((b,i)=>`Filter ${i+1}: ${b.enabled?'ON':'OFF'} ${code[b.band_type]} Fc ${b.freq_hz.toFixed(1)} Hz${gainTypes.has(b.band_type)?` Gain ${b.gain_db.toFixed(2)} dB`:''} Q ${b.q.toFixed(3)}`).join('\n')};
      }
    }
    return mockResponse();
  }
  const invoke = (command,args) => previewMode ? mockInvoke(command,args) : window.__TAURI__.core.invoke(command,args);
  function enqueue(task) { const next=queue.then(task); queue=next.catch(()=>{}); return next; }
  function toast(message,kind='success') {
    const el=document.createElement('div'); el.className=`toast ${kind}`; el.textContent=message; $('toasts').append(el);
    setTimeout(()=>el.remove(),kind==='error'?9000:4500);
  }
  function report(error) {
    const message=typeof error==='string'?error:error?.message||String(error);
    $('sync-status').textContent='Not synced · edit a setting to retry';
    if(message!==toastError) { toast(message,'error'); toastError=message; setTimeout(()=>{toastError='';},5000); }
  }
  function fillRange(el) { el.style.setProperty('--fill',`${100*(Number(el.value)-Number(el.min))/(Number(el.max)-Number(el.min))}%`); }
  function setRange(id,value) { $(id).value=value; fillRange($(id)); }
  function selectDevices(id,devices,value) {
    const select=$(id); select.replaceChildren();
    devices.forEach(device=>select.add(new Option(device.label,device.id)));
    if(!devices.some(d=>d.id===value)) select.add(new Option(value?`Unavailable · ${value}`:'No device selected',value||''));
    select.value=value;
  }
  function renderDevices() {
    selectDevices('capture',state.capture_devices||[],settings.capture_device);
    selectDevices('playback',state.playback_devices||[],settings.playback_device);
  }
  function renderProfiles() {
    $('profiles').replaceChildren();
    const profiles=state.profiles||[];
    if(!profiles.includes(settings.active_profile)) $('profiles').add(new Option(settings.active_profile||'Custom',settings.active_profile||''));
    profiles.forEach(name=>$('profiles').add(new Option(name,name)));
    $('profiles').value=settings.active_profile||'';
    $('delete-profile').disabled=!profiles.includes(settings.active_profile);
  }
  function renderSettings() {
    $('crossfeed-enabled').checked=settings.crossfeed_enabled;
    $('eq-enabled').checked=settings.eq_enabled; $('auto-preamp').checked=settings.auto_preamp;
    setRange('angle',settings.crossfeed_angle_deg); setRange('mid',settings.crossfeed_mid_boost_db); setRange('preamp',settings.preamp_db);
    $('sample-rate').value=settings.sample_rate; $('chunksize').value=settings.chunksize; $('exclusive').checked=settings.exclusive_mode;
    updateLabels(); renderDevices(); renderProfiles(); renderBands();
  }
  function updateLabels() {
    $('angle-value').innerHTML=`${settings.crossfeed_angle_deg.toFixed(0)}<span>°</span>`;
    $('mid-value').innerHTML=`${settings.crossfeed_mid_boost_db.toFixed(1)} <span>dB</span>`;
    $('preamp-value').innerHTML=`${signed(settings.preamp_db)} <span>dB</span>`;
    $('bypass').setAttribute('aria-pressed',String(settings.master_bypass)); $('bypass-label').textContent=settings.master_bypass?'Original signal':'DSP active';
    $('bypass-overlay').hidden=!settings.master_bypass;
    $('auto-label').textContent=settings.auto_preamp?'Auto-preamp on':'Manual gain · anti-clip off';
    $('headroom-status').textContent=settings.auto_preamp?'ANTI-CLIP':'MANUAL';
    $('headroom-status').style.color=settings.auto_preamp?'var(--ok)':'var(--warn)';
    $('crossfeed-card').classList.toggle('processing-off',!settings.crossfeed_enabled);
    $('exclusive-wrap').hidden=!(settings.capture_backend.toLowerCase()==='wasapi'||settings.playback_backend.toLowerCase()==='wasapi');
    $('latency').textContent=`${settings.sample_rate/1000} kHz / ${settings.chunksize} samples`;
    $('routing-summary').textContent=`${settings.sample_rate/1000} kHz · ${settings.chunksize} samples · ${(settings.chunksize/settings.sample_rate*1000).toFixed(1)} ms buffer`;
    document.querySelectorAll('[data-angle]').forEach(el=>el.classList.toggle('active',Number(el.dataset.angle)===settings.crossfeed_angle_deg));
  }
  function renderBands() {
    $('band-rows').innerHTML=settings.eq_bands.map((b,i)=>{
      const gainDisabled=!gainTypes.has(b.band_type);
      const cell=(key,min,max,step,log=false,disabled=false)=>`<td><input type="number" data-band="${i}" data-key="${key}" value="${key==='q'?Number(b[key].toFixed(3)):Number(b[key].toFixed(2))}" min="${min}" max="${max}" step="${step}" aria-label="Band ${i+1} ${key==='freq_hz'?'frequency in Hz':key==='gain_db'?'gain in dB':'Q'}" ${disabled?'disabled':''}><input type="range" data-band="${i}" data-key="${key}" ${log?'data-log="true"':''} min="${log?0:min}" max="${log?1000:max}" step="${log?1:step}" value="${log?Math.log(b[key]/min)/Math.log(max/min)*1000:b[key]}" aria-label="Band ${i+1} ${key} slider" ${disabled?'disabled':''}></td>`;
      return `<tr data-row="${i}" class="${b.enabled?'':'band-disabled'} ${selectedBand===i?'selected':''}"><td><button class="band-toggle" data-enable="${i}" style="--band-color:${colors[i]}" aria-label="Enable band ${i+1}" aria-pressed="${b.enabled}">${i+1}</button></td><td><select data-band="${i}" data-key="band_type" aria-label="Band ${i+1} filter type">${Object.entries(types).map(([key,label])=>`<option value="${key}" ${b.band_type===key?'selected':''}>${label}</option>`).join('')}</select></td>${cell('freq_hz',20,20000,1,true)}${cell('gain_db',-18,18,.1,false,gainDisabled)}${cell('q',.1,20,.01,true)}<td><button class="remove" data-remove="${i}" aria-label="Remove band ${i+1}" title="Remove band ${i+1}">×</button></td></tr>`;
    }).join('');
    $('band-rows').querySelectorAll('input[type=range]').forEach(fillRange);
    $('band-count').textContent=`${settings.eq_bands.length} / 12 bands`;
    $('add-band').disabled=settings.eq_bands.length>=12 || !ready;
    $('empty-bands').hidden=!!settings.eq_bands.length;
  }
  function syncBandRow(index) {
    const b=settings.eq_bands[index]; if(!b) return;
    $('band-rows').querySelectorAll(`[data-band="${index}"]`).forEach(el=>{
      const key=el.dataset.key; if(key==='band_type') return;
      if(el===document.activeElement && el.type==='number') return;
      el.value=el.dataset.log ? Math.log(b[key]/(key==='q'?.1:20))/Math.log(key==='q'?200:1000)*1000 : Number(b[key].toFixed(key==='q'?3:2));
      if(el.type==='range') fillRange(el);
    });
  }
  const graph={width:800,height:300,left:45,right:778,top:15,bottom:267};
  const gx=f=>graph.left+Math.log10(clamp(f,20,20000)/20)/3*(graph.right-graph.left);
  const gy=db=>graph.top+(18-clamp(db,-24,24))/36*(graph.bottom-graph.top);
  function pathFor(freq,values,x=gx,y=gy) { return freq.map((f,i)=>`${i?'L':'M'}${x(f).toFixed(2)},${y(Number.isFinite(values[i])?values[i]:0).toFixed(2)}`).join(' '); }
  function renderEQ(preview) {
    if(!preview?.freq_hz?.length) return;
    const ticks=[20,50,100,200,500,1000,2000,5000,10000,20000], levels=[-18,-12,-6,0,6,12,18];
    let html='<defs><linearGradient id="eq-fill" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="#b98af0" stop-opacity=".2"/><stop offset="100%" stop-color="#b98af0" stop-opacity=".015"/></linearGradient><clipPath id="plot-clip"><rect x="45" y="15" width="733" height="252"/></clipPath></defs>';
    levels.forEach(db=>{const y=gy(db);html+=`<line x1="45" x2="778" y1="${y}" y2="${y}" stroke="${db===0?'#83899775':'#ffffff09'}" ${db===0?'stroke-dasharray="3 5"':''}/><text x="33" y="${y+3}" text-anchor="end" fill="#838997" font-size="9">${db>0?'+':''}${db}</text>`;});
    ticks.forEach(f=>{const x=gx(f);html+=`<line x1="${x}" x2="${x}" y1="15" y2="267" stroke="#ffffff08"/><text x="${x}" y="287" text-anchor="middle" fill="#838997" font-size="9">${f>=1000?`${f/1000}k`:f}</text>`;});
    html+='<text x="14" y="7" fill="#838997" font-size="8">dB</text><g clip-path="url(#plot-clip)">';
    const compositePath=pathFor(preview.freq_hz,preview.composite_db);
    html+=`<path d="${compositePath} L${gx(20000)},${gy(0)} L${gx(20)},${gy(0)} Z" fill="url(#eq-fill)"/>`;
    (preview.per_band_db||[]).forEach((values,i)=>{if(settings.eq_bands[i]?.enabled && settings.eq_enabled) html+=`<path d="${pathFor(preview.freq_hz,values)}" fill="none" stroke="${colors[i]}" stroke-width="1.15" opacity="${selectedBand===i?.7:.3}"/>`;});
    html+=`<path d="${compositePath}" fill="none" stroke="#b98af0" stroke-width="2.5" stroke-linejoin="round" opacity="${settings.eq_enabled?1:.3}"/></g>`;
    settings.eq_bands.forEach((b,i)=>{
      if(!b.enabled) return;
      const y=gy(gainTypes.has(b.band_type)?b.gain_db:0),x=gx(b.freq_hz);
      html+=`<g class="handle" data-handle="${i}" opacity="${settings.eq_enabled?1:.4}"><circle cx="${x}" cy="${y}" r="19" fill="transparent"/>${selectedBand===i?`<circle cx="${x}" cy="${y}" r="14" fill="${colors[i]}" opacity=".12"/>`:''}<circle cx="${x}" cy="${y}" r="9" fill="#171b22" stroke="${colors[i]}" stroke-width="1.5"/><text x="${x}" y="${y+3}" text-anchor="middle" fill="${colors[i]}" font-size="9" pointer-events="none">${i+1}</text></g>`;
    });
    $('eq-graph').innerHTML=html;
    $('peak').innerHTML=`${signed(preview.raw_peak_db||0)} <small>dB</small>`;
    $('effective').innerHTML=`${signed(preview.effective_preamp_db||0)} <small>dB</small>`;
    $('effective').style.color=preview.effective_preamp_db+(preview.raw_peak_db||0)>0?'var(--warn)':'var(--ok)';
  }
  function renderStage() {
    const a=settings.crossfeed_angle_deg*Math.PI/180,cx=150,cy=111,r=91;
    const lx=cx-Math.sin(a)*r,rx=cx+Math.sin(a)*r,y=cy-Math.cos(a)*r;
    $('stage').innerHTML=`<defs><radialGradient id="stage-glow"><stop stop-color="#b98af0" stop-opacity=".09"/><stop offset="1" stop-color="#b98af0" stop-opacity="0"/></radialGradient></defs><ellipse cx="150" cy="92" rx="100" ry="66" fill="url(#stage-glow)"/><path d="M59 111 A91 91 0 0 1 241 111" fill="none" stroke="#ffffff12" stroke-dasharray="3 5"/><path d="M${lx} ${y} A91 91 0 ${settings.crossfeed_angle_deg>90?1:0} 1 ${rx} ${y}" stroke="#b98af044" stroke-width="2" fill="none"/><path d="M150 111 L${lx} ${y} M150 111 L${rx} ${y}" stroke="#b98af055" stroke-dasharray="3 4"/><path d="M150 111 V13" stroke="#ffffff14" stroke-dasharray="2 4"/><g fill="#201b2a" stroke="#b98af0"><rect x="${lx-9}" y="${y-12}" width="18" height="24" rx="4"/><rect x="${rx-9}" y="${y-12}" width="18" height="24" rx="4"/></g><g fill="none" stroke="#b98af0" opacity=".7"><circle cx="${lx}" cy="${y+3}" r="4"/><circle cx="${rx}" cy="${y+3}" r="4"/></g><text x="${lx}" y="${y-18}" fill="#b9bfc9" text-anchor="middle" font-size="11">L</text><text x="${rx}" y="${y-18}" fill="#b9bfc9" text-anchor="middle" font-size="11">R</text><circle cx="150" cy="111" r="13" fill="#24212d" stroke="#b98af0" stroke-width="1.5"/><path d="M146 101 L150 96 L154 101 M135 108 V116 M165 108 V116" fill="none" stroke="#b98af0" stroke-width="2"/><text x="150" y="73" text-anchor="middle" fill="#b98af0" font-size="12">±${settings.crossfeed_angle_deg}°</text>`;
  }
  function renderCross(preview) {
    if(!preview?.freq_hz?.length) return;
    const x=f=>25+Math.log10(f/20)/3*300, y=db=>clamp(70-db*2.2,4,84);
    let html='';
    [-12,0,12,24].forEach(db=>html+=`<line x1="25" x2="325" y1="${y(db)}" y2="${y(db)}" stroke="#ffffff0c"/><text x="19" y="${y(db)+3}" text-anchor="end" font-size="7" fill="#838997">${db}</text>`);
    [[preview.mid_db,'#b98af0'],[preview.side_db,'#7dd3c8'],[preview.ild_db,'#e8b34b']].forEach(([values,color])=>html+=`<path d="${pathFor(preview.freq_hz,values,x,y)}" fill="none" stroke="${color}" stroke-width="1.5"/>`);
    [20,1000,20000].forEach(f=>html+=`<text x="${x(f)}" y="98" text-anchor="middle" fill="#838997" font-size="7">${f>=1000?f/1000+'k':f}</text>`);
    $('cross-chart').innerHTML=html;
  }
  function drawLocal() { renderEQ(computePEQ(settings)); renderStage(); renderCross(computeCrossfeed(settings)); }
  function requestDraw() { if(!frame) frame=requestAnimationFrame(()=>{frame=null;drawLocal();}); }
  function renderMeters(meters) {
    if(!meters) return;
    state.meters=meters;
    for(const [id,peakKey,rmsKey] of [['input-meters','capture_peak','capture_rms'],['output-meters','playback_peak','playback_rms']]) {
      const peaks=meters[peakKey]||[-120,-120], rms=meters[rmsKey]||peaks;
      $(id).querySelectorAll('.stereo i').forEach((bar,i)=>{
        const db=meters.running ? (peaks[i]??-120) : -120;
        bar.querySelector('b').style.width=`${clamp((db+60)/60*100,0,100)}%`;
        bar.querySelector('em').style.left=`${clamp(((meters.running?rms[i]:-120)+60)/60*100,0,100)}%`;
        bar.title=`${i?'R':'L'} peak ${db.toFixed(1)} dBFS · RMS ${(rms[i]??-120).toFixed(1)} dBFS`;
      });
      const max=meters.running?Math.max(...peaks):-120;
      $(id).querySelector('output').textContent=max<=-100?'−∞':max.toFixed(0);
      $(id).setAttribute('aria-label',`${id==='input-meters'?'Input':'Output'} peak ${max<=-100?'silent':max.toFixed(1)+' dBFS'}`);
    }
    $('cpu').textContent=`${Number(meters.load_percent||0).toFixed(1)}%`;
    const clips=Number(meters.clipped_samples||0); if(clips<clipAcknowledged) clipAcknowledged=0;
    $('clip').classList.toggle('is-clipped',clips>clipAcknowledged); $('clip').querySelector('span').textContent=clips.toLocaleString();
    $('clip').title=`${clips} clipped samples. Click to acknowledge the warning.`;
    $('engine-status').textContent=meters.error?'● Engine error':meters.running?`● Running ${settings.sample_rate/1000} kHz`:`● ${meters.state||'Stopped'}`;
    $('engine-status').className=`status ${meters.error?'error':meters.running?'running':''}`;
    $('engine').innerHTML=meters.running?'Stop DSP <span>■</span>':'Start DSP <span>▶</span>';
    $('engine').disabled=!ready || engineBusy;
    if(meters.error) report(meters.error);
    if(meters.running && !pollTimer) startPolling();
    if(!meters.running && pollTimer) stopPolling();
  }
  function startPolling() {
    const generation=++pollGeneration;
    pollTimer=setInterval(async()=>{
      if(pollBusy || document.hidden || actionDepth) return;
      pollBusy=true;
      try { const meters=await invoke('get_live_meters'); if(generation===pollGeneration) renderMeters(meters); }
      catch(error) { if(generation===pollGeneration) report(error); }
      finally { pollBusy=false; }
    },200);
  }
  function stopPolling() { clearInterval(pollTimer); pollTimer=null; pollGeneration++; }
  function applyResponse(response,expectedRevision,full=false) {
    if(!response?.settings) throw Error('The audio engine returned an incomplete state.');
    // Never overwrite newer local edits with a slower backend response.
    if(expectedRevision!==revision) { renderMeters(response.meters); return; }
    state=response; settings=clone(response.settings);
    if(full) renderSettings(); else { updateLabels(); syncAllBandRows(); }
    renderEQ(response.peq_preview||computePEQ(settings)); renderStage(); renderCross(response.crossfeed_preview||computeCrossfeed(settings));
    renderMeters(response.meters);
    $('sync-status').textContent=previewMode?'Preview · changes simulated locally':'All changes applied';
  }
  function syncAllBandRows() { settings.eq_bands.forEach((_,i)=>syncBandRow(i)); }
  function edited({bands=false}={}) {
    if(!ready) return;
    revision++; $('dirty').hidden=false; updateLabels(); if(bands) renderBands();
    requestDraw(); $('sync-status').textContent=previewMode?'Updating preview…':'Applying changes…';
    // Coalesce the next 80 ms of edits without starving live audio during a
    // continuous drag. The last edit always gets its own trailing update.
    if(!updateTimer) updateTimer=setTimeout(sendUpdate,80);
  }
  function sendUpdate() {
    clearTimeout(updateTimer); updateTimer=null;
    const snapshot=clone(settings),expected=revision;
    return enqueue(async()=>{
      // A slow backend can accumulate work: only send the newest waiting edit.
      if(expected!==revision) return;
      actionDepth++;
      try { applyResponse(await invoke('update_dsp_settings',{settings:snapshot}),expected); }
      catch(error) { report(error); }
      finally { actionDepth--; }
    });
  }
  async function action(command,args={},full=true) {
    if(!ready) throw Error('Wait for the studio to finish connecting.');
    if(updateTimer) sendUpdate();
    return enqueue(async()=>{
      // Snapshot at execution time so any preceding queued update has settled.
      const expected=revision;
      actionDepth++;
      try {
        if(command==='start_engine') args={settings:clone(settings)};
        const response=await invoke(command,args);
        if(response.settings) applyResponse(response,expected,full);
        return response;
      } finally { actionDepth--; }
    });
  }

  // Global controls: local feedback first, ordered/debounced audio updates second.
  [['angle','crossfeed_angle_deg'],['mid','crossfeed_mid_boost_db'],['preamp','preamp_db']].forEach(([id,key])=>{
    $(id).addEventListener('input',event=>{settings[key]=Number(event.target.value);fillRange(event.target);edited();});
  });
  [['crossfeed-enabled','crossfeed_enabled'],['eq-enabled','eq_enabled'],['auto-preamp','auto_preamp'],['exclusive','exclusive_mode']].forEach(([id,key])=>$(id).addEventListener('change',event=>{settings[key]=event.target.checked;edited();}));
  $('bypass').addEventListener('click',()=>{settings.master_bypass=!settings.master_bypass;edited();});
  $('angle-presets').addEventListener('click',event=>{const button=event.target.closest('[data-angle]');if(button){settings.crossfeed_angle_deg=Number(button.dataset.angle);setRange('angle',settings.crossfeed_angle_deg);edited();}});
  [['sample-rate','sample_rate'],['chunksize','chunksize']].forEach(([id,key])=>$(id).addEventListener('change',event=>{settings[key]=Number(event.target.value);edited();}));
  ['capture','playback'].forEach(direction=>$(direction).addEventListener('change',event=>{
    const device=(state[`${direction}_devices`]||[]).find(d=>d.id===event.target.value); if(!device) return;
    settings[`${direction}_device`]=device.id; settings[`${direction}_backend`]=device.backend;
    if(direction==='capture') settings.capture_loopback=device.is_loopback;
    edited();
  }));
  $('refresh').addEventListener('click',async()=>{
    $('refresh').disabled=true;
    try { await action('refresh_devices');toast('Audio devices refreshed.'); } catch(error) { report(error); } finally { $('refresh').disabled=false; }
  });
  $('engine').addEventListener('click',async()=>{
    if(engineBusy) return; engineBusy=true; $('engine').disabled=true;
    try { await action(state.meters?.running?'stop_engine':'start_engine'); if(previewMode) toast(state.meters.running?'Preview meters started. No audio is captured or played.':'Preview stopped.'); }
    catch(error) { report(error); }
    finally {engineBusy=false; $('engine').disabled=!ready;}
  });
  $('clip').addEventListener('click',()=>{clipAcknowledged=state?.meters?.clipped_samples||0; $('clip').classList.remove('is-clipped');});
  $('add-band').addEventListener('click',()=>{
    if(settings.eq_bands.length>=12) return;
    settings.eq_bands.push({enabled:true,band_type:'bell',freq_hz:1000,gain_db:0,q:1}); selectedBand=settings.eq_bands.length-1;
    edited({bands:true}); $('band-rows').lastElementChild?.scrollIntoView({block:'nearest'});
  });
  $('reset').addEventListener('click',()=>{
    settings.eq_bands.forEach(b=>{b.gain_db=0;if(!gainTypes.has(b.band_type)) b.enabled=false;});
    edited({bands:true});toast('EQ flattened. Band positions are kept; pass and notch filters are disabled.');
  });
  $('band-rows').addEventListener('click',event=>{
    const toggle=event.target.closest('[data-enable]'),remove=event.target.closest('[data-remove]');
    if(toggle) {const index=Number(toggle.dataset.enable);settings.eq_bands[index].enabled=!settings.eq_bands[index].enabled;selectedBand=index;edited({bands:true});}
    if(remove) {settings.eq_bands.splice(Number(remove.dataset.remove),1);selectedBand=-1;edited({bands:true});}
  });
  function bandInput(event) {
    const el=event.target,index=Number(el.dataset.band),key=el.dataset.key;
    if(!key || !settings.eq_bands[index]) return;
    selectedBand=index;
    if(key==='band_type') {if(event.type!=='change') return;settings.eq_bands[index][key]=el.value;edited({bands:true});return;}
    if(event.type==='change' && el.type==='range') return;
    const value=Number(el.value);
    if(el.value.trim()==='' || !Number.isFinite(value) || value<Number(el.min) || value>Number(el.max)) {
      if(event.type==='change') {el.value=settings.eq_bands[index][key];el.setCustomValidity('');} return;
    }
    const next=el.dataset.log ? (key==='q'?.1:20)*(key==='q'?200:1000)**(value/1000) : value;
    settings.eq_bands[index][key]=key==='freq_hz'?Math.round(next):Math.round(next*1000)/1000;
    syncBandRow(index);if(el.type==='range')fillRange(el);edited();
  }
  $('band-rows').addEventListener('input',bandInput);$('band-rows').addEventListener('change',bandInput);
  $('band-rows').addEventListener('focusin',event=>{const row=event.target.closest('[data-row]');if(row){selectedBand=Number(row.dataset.row);requestDraw();}});

  // SVG drag math is independent of CSS pixel size, including non-uniform resizing.
  function graphPoint(event) {
    const rect=$('eq-graph').getBoundingClientRect();
    return {x:(event.clientX-rect.left)/rect.width*800,y:(event.clientY-rect.top)/rect.height*300};
  }
  function selectInfo(index) {
    const b=settings.eq_bands[index]; if(!b) return;
    const freq=b.freq_hz>=1000?`${(b.freq_hz/1000).toFixed(2)} kHz`:`${b.freq_hz.toFixed(0)} Hz`;
    const info=`Band ${index+1} · ${freq}${gainTypes.has(b.band_type)?` · ${signed(b.gain_db)} dB`:''} · Q ${b.q.toFixed(2)}`;
    $('selected-info').textContent=info; $('graph-tooltip').textContent=info;
  }
  $('eq-graph').addEventListener('pointerdown',event=>{
    const handle=event.target.closest('[data-handle]'); if(!handle || !ready || event.button!==0) return;
    event.preventDefault();selectedBand=Number(handle.dataset.handle);drag={index:selectedBand,pointerId:event.pointerId};
    $('eq-graph').setPointerCapture(event.pointerId);$('graph-tooltip').hidden=false; selectInfo(selectedBand);requestDraw();
  });
  $('eq-graph').addEventListener('pointermove',event=>{
    if(!drag || drag.pointerId!==event.pointerId) return;
    const point=graphPoint(event),b=settings.eq_bands[drag.index]; if(!b) return;
    b.freq_hz=Math.round(20*1000**clamp((point.x-graph.left)/(graph.right-graph.left),0,1));
    if(gainTypes.has(b.band_type)) b.gain_db=Math.round(clamp(18-(point.y-graph.top)/(graph.bottom-graph.top)*36,-18,18)*10)/10;
    syncBandRow(drag.index);selectInfo(drag.index);edited();
  });
  function finishDrag(event) {
    if(!drag || (event && event.pointerId!==drag.pointerId)) return;
    const pointer=drag.pointerId;drag=null;$('graph-tooltip').hidden=true;
    if($('eq-graph').hasPointerCapture(pointer)) $('eq-graph').releasePointerCapture(pointer);
    if(updateTimer) sendUpdate();
  }
  ['pointerup','pointercancel','lostpointercapture'].forEach(name=>$('eq-graph').addEventListener(name,finishDrag));
  $('eq-graph').addEventListener('wheel',event=>{
    const handle=event.target.closest('[data-handle]');if(!handle || !ready)return;
    event.preventDefault();selectedBand=Number(handle.dataset.handle);const b=settings.eq_bands[selectedBand];
    b.q=Math.round(clamp(b.q*(event.deltaY<0?1.08:1/1.08),.1,20)*100)/100;
    syncBandRow(selectedBand);selectInfo(selectedBand);edited();
  },{passive:false});

  // Native dialog elements provide keyboard focus trapping and Escape dismissal.
  document.querySelectorAll('[data-close]').forEach(button=>button.addEventListener('click',()=>button.closest('dialog').close()));
  document.querySelectorAll('dialog').forEach(dialog=>dialog.addEventListener('click',event=>{if(event.target===dialog){const r=dialog.getBoundingClientRect();if(event.clientX<r.left || event.clientX>r.right || event.clientY<r.top || event.clientY>r.bottom)dialog.close();}}));
  function openProfile(actionType) {
    profileAction=actionType;$('profile-error').textContent='';const deleting=actionType==='delete';
    $('profile-dialog-title').textContent=deleting?'Delete profile?':'Save listening profile';
    $('profile-description').textContent=deleting?'This removes the saved profile. Your current listening settings will stay unchanged.':'Keep your EQ, crossfeed and gain settings together. An existing name will replace that saved profile.';
    $('profile-name').value=deleting?settings.active_profile:'';$('profile-name').readOnly=deleting;
    $('profile-submit').textContent=deleting?'Delete profile':'Save profile';
    $('profile-submit').style.background=deleting?'var(--danger)':'';$('profile-dialog').showModal();
    if(!deleting)$('profile-name').focus();
  }
  $('save-profile').addEventListener('click',()=>openProfile('save'));
  $('delete-profile').addEventListener('click',()=>openProfile('delete'));
  $('profile-form').addEventListener('submit',async event=>{
    event.preventDefault(); const name=$('profile-name').value.trim();if(!name){$('profile-error').textContent='Please enter a profile name.';return;}
    $('profile-submit').disabled=true;$('profile-error').textContent='';
    try {await action(profileAction==='delete'?'delete_profile':'save_profile',{name});$('dirty').hidden=true;$('profile-dialog').close();toast(profileAction==='delete'?'Profile deleted.':`Saved “${name}”.`);}
    catch(error){$('profile-error').textContent=error?.message||String(error);}
    finally{$('profile-submit').disabled=false;}
  });
  $('profiles').addEventListener('change',async event=>{
    const name=event.target.value;$('profiles').disabled=true;
    try {await action('load_profile',{name});selectedBand=-1;$('dirty').hidden=true;toast(`Loaded “${name}”.`);}
    catch(error){report(error);renderProfiles();}finally{$('profiles').disabled=false;}
  });
  $('autoeq').addEventListener('click',()=>{$('autoeq-error').textContent='';$('autoeq-dialog').showModal();});
  $('export').addEventListener('click',async()=>{
    $('export').disabled=true;
    try {const response=await action('export_autoeq',{},false);$('autoeq-text').value=response.text;$('autoeq-error').textContent='';toast('Current EQ exported. Ready to copy.');}
    catch(error){$('autoeq-error').textContent=error?.message||String(error);}finally{$('export').disabled=false;}
  });
  $('import').addEventListener('click',async()=>{
    const text=$('autoeq-text').value.trim();if(!text){$('autoeq-error').textContent='Paste an AutoEQ / EqualizerAPO preset first.';return;}
    $('import').disabled=true;
    try {await action('import_autoeq',{text});selectedBand=-1;$('dirty').hidden=false;$('autoeq-dialog').close();toast('AutoEQ imported. Your previous EQ has been replaced.');}
    catch(error){$('autoeq-error').textContent=error?.message||String(error);}finally{$('import').disabled=false;}
  });
  $('copy').addEventListener('click',async()=>{
    if(!$('autoeq-text').value.trim()){$('autoeq-error').textContent='Export your current EQ or paste a preset first.';return;}
    try {await navigator.clipboard.writeText($('autoeq-text').value);toast('Filter text copied to clipboard.');}
    catch{$('autoeq-text').focus();$('autoeq-text').select();$('autoeq-error').textContent='Clipboard is unavailable here. Text selected — press Ctrl+C (or ⌘C) to copy.';}
  });
  window.addEventListener('pagehide',()=>{stopPolling();clearTimeout(updateTimer);});
  window.addEventListener('pageshow',()=>{if(ready && state.meters?.running && !pollTimer)startPolling();});

  async function init() {
    $('preview-badge').hidden=!previewMode;
    if(previewMode) {
      $('footer-mode').textContent='BROWSER PREVIEW · SIMULATED METERS · NO AUDIO';
      $('cross-chart').setAttribute('aria-label','Approximate browser crossfeed response; not measured KU100 HRTF data');
      $('cross-chart').parentElement.title='Browser preview uses an analytical crossfeed approximation. Native mode displays the backend KU100 response.';
    }
    try {
      state=await invoke('get_app_state');ready=true;
      applyResponse(state,revision,true);$('app').setAttribute('aria-busy','false');$('engine').disabled=false;
    } catch(error) {
      $('app').setAttribute('aria-busy','false');$('engine-status').textContent='● Connection failed';$('engine-status').className='status error';
      report(error);$('sync-status').textContent='Audio engine unavailable. Restart the app to reconnect.';
      document.querySelectorAll('button,input,select').forEach(el=>el.disabled=true);
      // A native bridge failure must never silently look like a working mock engine.
    }
  }
  init();
})();
