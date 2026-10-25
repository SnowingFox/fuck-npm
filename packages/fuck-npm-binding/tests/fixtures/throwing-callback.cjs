// Usage: node throwing-callback.cjs <binding dir>. Prints a JSON report.
const binding = require(process.argv[2]);
const uncaught = [];
let calls = 0;
process.on("uncaughtException", (error) => uncaught.push(error.message));
binding
  .__testDeliver([[{ type: "link", done: 1, total: 3 }], [{ type: "link", done: 2, total: 3 }], [{ type: "link", done: 3, total: 3 }]], () => {
    calls += 1;
    if (calls === 2) throw new Error("listener blew up");
  })
  .then((settled) => {
    process.stdout.write(JSON.stringify({ uncaught, settled, calls }));
  });
