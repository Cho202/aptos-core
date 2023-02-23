from aptos_sdk.account import Account
from aptos_sdk.client import FaucetClient, RestClient
import argparse
import logging
import time, threading


LOG = logging.getLogger(__name__)
formatter = logging.Formatter("%(asctime)s - %(levelname)s - %(message)s")
ch = logging.StreamHandler()
ch.setFormatter(formatter)
LOG.addHandler(ch)


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("--node-url", default="http://127.0.0.1:8080/v1")
    parser.add_argument("--faucet-url", default="http://127.0.0.1:10212")
    parser.add_argument("--num-simultaneous-requests", type=int, default=1000)
    parser.add_argument("-d", "--debug", action="store_true")
    args = parser.parse_args()
    return args


lock = threading.Lock()

def main():
    args = parse_args()

    if args.debug:
        LOG.setLevel("DEBUG")
    else:
        LOG.setLevel("INFO")

    alice = Account.generate().address()

    global outstanding, success, fail
    outstanding = args.num_simultaneous_requests
    success = 0
    fail = 0

    rest_client = RestClient(args.node_url)
    faucet_client = FaucetClient(args.faucet_url, rest_client)

    total = outstanding
    idx = 1

    while idx <= total:
        thread = threading.Thread(target=request, args=(faucet_client, alice, idx))
        thread.start()
        idx += 1

    while outstanding > 1:
        print(outstanding)
        time.sleep(1)

    print("Succeeded: " + str(success))
    print("Failed:" + str(fail))


def do_request(faucet_client, address, amount):
    try:
        faucet_client.fund_account(address, amount)
    except:
        return False
    return True


def request(faucet_client, address, amount):
    global lock, outstanding, success, fail
    result = do_request(faucet_client, address, amount)
    with lock:
        outstanding -= 1
        if result:
            success += 1
        else:
            fail += 1


if __name__ == "__main__":
    main()

