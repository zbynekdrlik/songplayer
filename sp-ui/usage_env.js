// #256: the two browser facts the usage log reads (`src/usage.rs`) that
// web-sys does not bind (sp-ui has no `js-sys` dependency).

// An automated session: every Playwright run sets it, the post-deploy specs
// on the boxes among them. Such a session records nothing.
export function webdriver() {
  return navigator.webdriver === true;
}

// The browser's clock (UTC unix ms), the stamp of each recorded use.
export function dateNow() {
  return Date.now();
}
