const assert = require("node:assert/strict");
const { paginate } = require("./src/paging.js");

// Test edge cases
console.log("Testing additional edge cases...");

// Test with empty array
assert.deepEqual(paginate([], 3), []);
console.log("✓ Empty array test passed");

// Test with single item
assert.deepEqual(paginate([1], 3), [[1]]);
console.log("✓ Single item test passed");

// Test with items exactly matching page size
assert.deepEqual(paginate([1, 2, 3], 3), [[1, 2, 3]]);
console.log("✓ Exact page size test passed");

// Test with one extra item
assert.deepEqual(paginate([1, 2, 3, 4], 3), [[1, 2, 3], [4]]);
console.log("✓ One extra item test passed");

// Test with two extra items
assert.deepEqual(paginate([1, 2, 3, 4, 5], 3), [[1, 2, 3], [4, 5]]);
console.log("✓ Two extra items test passed");

console.log("All additional tests passed!");