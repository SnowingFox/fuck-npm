// Usage: node exits.cjs <binding dir>. Must exit on its own once the promise settles.
const binding = require(process.argv[2]);
binding.__testDeliver([[{ type: "link", done: 1, total: 1 }]], () => {}).then(() => console.log("settled"));
