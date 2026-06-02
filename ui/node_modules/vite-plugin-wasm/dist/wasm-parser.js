"use strict";
var __importDefault = (this && this.__importDefault) || function (mod) {
    return (mod && mod.__esModule) ? mod : { "default": mod };
};
Object.defineProperty(exports, "__esModule", { value: true });
exports.parseWasm = parseWasm;
exports.generateGlueCode = generateGlueCode;
const fs_1 = __importDefault(require("fs"));
async function parseWasm(wasmFilePath) {
    try {
        const wasmBinary = await fs_1.default.promises.readFile(wasmFilePath);
        const wasmModule = await WebAssembly.compile(wasmBinary);
        const imports = Object.entries(WebAssembly.Module.imports(wasmModule).reduce((result, item) => ({
            ...result,
            [item.module]: [...(result[item.module] || []), item.name]
        }), {})).map(([from, names]) => ({ from, names }));
        const exports = WebAssembly.Module.exports(wasmModule).map(item => item.name);
        return { imports, exports };
    }
    catch (e) {
        throw new Error(`Failed to parse WASM file: ${e.message}`);
    }
}
async function generateGlueCode(wasmFilePath, names) {
    const { imports, exports } = await parseWasm(wasmFilePath);
    const importStatements = imports.map(({ from }, i) => {
        return `import * as __vite__wasmImport_${i} from ${JSON.stringify(from)};`;
    });
    const importObject = imports.map(({ from, names }, i) => {
        return {
            key: JSON.stringify(from),
            value: names.map(name => {
                return {
                    key: JSON.stringify(name),
                    value: `__vite__wasmImport_${i}[${JSON.stringify(name)}]`
                };
            })
        };
    });
    const initCode = `const __vite__wasmModule = await ${names.initWasm}(${codegenSimpleObject(importObject)}, ${names.wasmUrl});`;
    const exportsStatements = [];
    const nameMap = new Map();
    // export const { a, b, c } = __vite__wasmModule;
    // const { "invalid-name": __vite__wasmExport_0 } = __vite__wasmModule; export { __vite__wasmExport_0 as "invalid-name" };
    exports.forEach((name, index) => {
        if (isValidJsDecalreName(name)) {
            exportsStatements.push(`  ${name},`);
        }
        else {
            const placeholderName = `__vite__wasmExport_${index}`;
            const exportName = JSON.stringify(name);
            exportsStatements.push(`  ${exportName}: ${placeholderName},`);
            nameMap.set(name, placeholderName);
        }
    });
    if (nameMap.size > 0) {
        exportsStatements.unshift(`const {`);
        exportsStatements.push(`} = __vite__wasmModule;`);
        exportsStatements.push(`export {`);
        exports.forEach(name => {
            const localName = nameMap.get(name);
            if (localName) {
                exportsStatements.push(`  ${localName} as ${JSON.stringify(name)},`);
            }
            else {
                exportsStatements.push(`  ${name},`);
            }
        });
        exportsStatements.push(`};`);
    }
    else {
        exportsStatements.unshift(`export const {`);
        exportsStatements.push(`} = __vite__wasmModule;`);
    }
    return [...importStatements, initCode, ...exportsStatements].join("\n");
}
function codegenSimpleObject(obj) {
    return `{ ${codegenSimpleObjectKeyValue(obj)} }`;
}
function codegenSimpleObjectKeyValue(obj) {
    return obj
        .map(({ key, value }) => {
        return `${key}: ${typeof value === "string" ? value : codegenSimpleObject(value)}`;
    })
        .join(",\n");
}
const VALID_JS_IDENTIFIER = /^[$_\p{ID_Start}][$\p{ID_Continue}]*$/u;
function isValidJsDecalreName(name) {
    return !isReservedWord(name) && VALID_JS_IDENTIFIER.test(name);
}
function isReservedWord(name) {
    switch (name) {
        case "abstract":
        case "boolean":
        case "break":
        case "byte":
        case "case":
        case "catch":
        case "char":
        case "class":
        case "const":
        case "continue":
        case "debugger":
        case "default":
        case "delete":
        case "do":
        case "double":
        case "else":
        case "enum":
        case "export":
        case "extends":
        case "false":
        case "final":
        case "finally":
        case "float":
        case "for":
        case "function":
        case "goto":
        case "if":
        case "implements":
        case "import":
        case "in":
        case "instanceof":
        case "int":
        case "interface":
        case "let":
        case "long":
        case "native":
        case "new":
        case "null":
        case "package":
        case "package":
        case "private":
        case "protected":
        case "public":
        case "return":
        case "short":
        case "static":
        case "super":
        case "switch":
        case "synchronized":
        case "this":
        case "throw":
        case "throws":
        case "transient":
        case "true":
        case "try":
        case "typeof":
        case "var":
        case "void":
        case "volatile":
        case "while":
        case "with":
        case "yield":
            return true;
    }
    return false;
}
