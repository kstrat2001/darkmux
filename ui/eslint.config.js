import reactHooks from "eslint-plugin-react-hooks";
import tseslint from "typescript-eslint";

export default tseslint.config({
  files: ["src/**/*.{ts,tsx}", "*.ts"],
  ignores: ["src/types/generated/**"],
  languageOptions: {
    parser: tseslint.parser,
    parserOptions: {
      projectService: { allowDefaultProject: ["devProxyTarget.test.ts"] },
      tsconfigRootDir: import.meta.dirname,
    },
  },
  plugins: { "@typescript-eslint": tseslint.plugin, "react-hooks": reactHooks },
  linterOptions: { reportUnusedDisableDirectives: "error" },
  rules: {
    "react-hooks/rules-of-hooks": "error",
    "react-hooks/exhaustive-deps": "warn",
    "@typescript-eslint/no-explicit-any": "error",
    "@typescript-eslint/no-unsafe-argument": "error",
    "@typescript-eslint/no-unsafe-assignment": "error",
    "@typescript-eslint/no-unsafe-call": "error",
    "@typescript-eslint/no-unsafe-member-access": "error",
    "@typescript-eslint/no-unsafe-return": "error",
    "no-restricted-syntax": [
      "error",
      {
        selector: "CallExpression[callee.object.name='Math'][callee.property.name=/^(max|min)$/] > SpreadElement",
        message: "A spread into Math.max/Math.min throws a RangeError on a large array. Use latestByTime/earliestByTime for record times, or maxOf/minOf.",
      },
    ],
  },
});
