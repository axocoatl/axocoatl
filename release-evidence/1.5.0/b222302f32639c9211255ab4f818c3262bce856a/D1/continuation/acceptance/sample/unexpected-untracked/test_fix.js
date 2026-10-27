const { paginate } = require('./src/paging.js');

// Test cases
console.log('Test 1: [1,2,3,4] with size 2');
console.log(paginate([1, 2, 3, 4], 2));

console.log('\nTest 2: [1,2,3,4,5] with size 2 (the failing case)');
console.log(paginate([1, 2, 3, 4, 5], 2));

console.log('\nTest 3: [1,2,3,4,5,6,7] with size 3');
console.log(paginate([1, 2, 3, 4, 5, 6, 7], 3));

console.log('\nTest 4: Empty array');
console.log(paginate([], 2));