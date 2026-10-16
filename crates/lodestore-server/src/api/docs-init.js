const element = document.getElementById("api-reference");
const configuration = JSON.parse(element.dataset.configuration);
let theme = null;
try {
  theme = localStorage.getItem("lodestore.theme");
} catch {}
if (theme !== "light" && theme !== "dark") {
  theme = matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}
configuration.forceDarkModeState = theme;
configuration.servers = [{ url: location.origin }];
element.dataset.configuration = JSON.stringify(configuration);
