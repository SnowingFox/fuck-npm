/** Bun accepts its disposable String paths; Node requires primitive strings. */
import path from "node:path";
export const { sep, delimiter, win32, posix, format, matchesGlob } = path;

function acceptingBoxedPaths<T extends (...args: any[]) => any>(fn: T): T {
  return ((...args: Parameters<T>) => fn(...args.map(arg => arg instanceof String ? String(arg) : arg))) as T;
}

export const join = acceptingBoxedPaths(path.join);
export const resolve = acceptingBoxedPaths(path.resolve);
export const relative = acceptingBoxedPaths(path.relative);
export const dirname = acceptingBoxedPaths(path.dirname);
export const basename = acceptingBoxedPaths(path.basename);
export const extname = acceptingBoxedPaths(path.extname);
export const normalize = acceptingBoxedPaths(path.normalize);
export const isAbsolute = acceptingBoxedPaths(path.isAbsolute);
export const parse = acceptingBoxedPaths(path.parse);
export const toNamespacedPath = acceptingBoxedPaths(path.toNamespacedPath);
export default { ...path, join, resolve, relative, dirname, basename, extname, normalize, isAbsolute, parse, toNamespacedPath };
