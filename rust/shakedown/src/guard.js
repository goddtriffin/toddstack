// The guard (design section 2): runs in every frame, popup and worker target before its first script, injected by
// `guard.rs` over its own CDP connection. Nothing may leave the machine over WebRTC and nothing may open a window.
// A violation is reported through a CDP binding captured here before page code runs, and removed from the page, so
// page code can neither suppress nor replace it (the console is not involved). `__BINDING__` is replaced by the tool.
(() => {
  const report = window['__BINDING__'];
  try { delete window['__BINDING__']; } catch { /* kept: nothing can be done */ }
  const send = (what) => { try { report(what); } catch { /* no binding: the tool's popup watch is the net */ } };
  const lock = (n, value) => {
    try { Object.defineProperty(window, n, { value, writable: false, configurable: false }); } catch { /* already locked */ }
  };
  for (const n of ['RTCPeerConnection', 'webkitRTCPeerConnection', 'RTCDataChannel']) {
    lock(n, function () {
      send('webrtc');
      throw new DOMException('WebRTC is blocked: nothing leaves the machine', 'NotAllowedError');
    });
  }
  lock('open', function () { send('popup'); return null; });
})();
