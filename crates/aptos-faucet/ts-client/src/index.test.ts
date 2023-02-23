import { AptosTapClient } from "./AptosTapClient";

test("node url empty", async () => {
  const client = new AptosTapClient({BASE: "http://127.0.0.1:10212"});
  const response = await client.default.root();
  expect(response).toBe("tap:ok");
});
