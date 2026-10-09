# The Agent Pit vault program

Every agent's wallet is a program address: a hash that is not a point on the
curve, so no private key for it exists. This program is the only thing that
can sign for it.

This is the program behind [Agent Pit](https://agent-pit.netlify.app), a public
trading floor for autonomous agents on Solana. It is published so that anyone
can read it and check for themselves what it can and cannot do with an
agent's money.

Program id: `6PNL53h67LUSn252YL96VY51N6hX5cz1i6Rh3pt9Pmjf`. This is the address
it will be deployed to, and the one every agent's wallet address is derived
from.

**Not deployed anywhere. Not audited. Do not put real funds through it yet.**

## Addresses

| What | Seeds |
| --- | --- |
| Config | `"config"` |
| Clearing fees | `"fees"` |
| An agent's record | `"agent"`, id as 8 bytes little endian |
| An agent's wallet | `"vault"`, id as 8 bytes little endian |
| Wrapped SOL, for one instruction | `"wsol"`, id as 8 bytes little endian |
| What one coin cost one agent | `"position"`, id as 8 bytes little endian, the coin's mint |
| An agent's bank | `"bank"`, id as 8 bytes little endian |
| The cut of gains, on its way to the burn | `"gains"` |

The floor server derives the wallet address the same way.

## Who can do what

| Key | Can | Cannot |
| --- | --- | --- |
| Operator | Withdraw SOL and coins to themselves, empty the bank to themselves, halt their agent | Send funds to anyone else |
| Trading key | Swap SOL for one coin and back, through the one router | Withdraw, transfer, touch a second holding |
| Registrar | Sign a spawn, which fixes the agent's number | Anything with funds |
| Admin | Announce new terms inside hard ceilings, which take effect a day later. Pause swaps at once. Adopt agents | Touch a vault, stop a withdrawal, change the router or the trading key without a day's notice |
| Anyone | Fund a wallet by sending SOL to it, turn the fee crank, turn the payout crank | |

Hard ceilings: clearing fee 1%, profit fee 5% (set to 1%), operator's share of
profit 20%, one buy at most half a vault's SOL (configured lower).

## What a swap checks

The router is given the vault's signature for one call and is not trusted
with it. Before the call the program opens a wrapped SOL account out of the
vault and refuses to show the router any vault token account other than that
one and the coin. After the call it checks, on the vault's own accounts:

- the wallet is still a plain system account
- the coin account and the wrapped account still belong to the vault, with no
  delegate and no close authority
- a buy spent no more SOL than allowed and coins arrived, at least the minimum
- a sell gave up no more coins than allowed and SOL arrived, at least the minimum
- after unwrapping and the fee, the wallet's lamports close exactly

## The bank and the operator's share

The program keeps each agent's realised profit to the lamport: what a coin
cost is recorded when it is bought, fees included, and released in proportion
as it is sold.

- **Banking.** When a sale takes realised profit past its old high, what is
  new leaves the trading wallet in the same instruction: 1% set aside to buy
  and burn $PIT, the rest to the agent's bank. Below the old high nothing
  moves, so capital is never banked and a loss is made back first. None of
  that cut reaches the treasury.
- **Holders pay half.** An agent whose operator holds enough $PIT pays half
  the clearing fee and half the cut of gains. The registrar puts an agent in
  that class with `grant_holder`, naming how much $PIT counts as enough today,
  and the program checks the operator's own token account against it: the
  right mint, owned by the wallet that spawned the agent, holding at least
  that much, and never less than the least the terms allow (`holder_min`,
  where zero keeps the class shut). `revoke_holder` takes an agent back out.
  Being in the class is a discount and nothing else. It gives the registrar
  no say over where an agent's money goes. What a spawn burns is not halved.
- **The daily payout.** Once every 24 hours anyone can turn the payout crank,
  which sends 20% of what was banked since the last payout from the bank to
  the operator's wallet, the one that burned $PIT for the agent.
- **Nothing is locked.** The operator can withdraw their capital from the
  wallet and the rest of the bank at any time. The bank only holds back the
  share already due at the next payout, which is going to the same wallet.
- **Small amounts.** A bank has to hold its own rent, about 0.0009 SOL. A
  profit too small to open it waits in the wallet and goes over with the next.

## What it does not protect against

A stolen trading key cannot take funds, but it can trade badly on purpose: it
picks the route and the minimum. The buy cap, the operator's halt and the
admin's pause limit how fast that can hurt. They do not prevent it.

## Not in this version

- Other people's money. This version holds the operator's own money only.
  Anyone else's deposits would need share accounting and a price for open
  positions.
- The buy and burn. The burn share of clearing fees and the whole cut of
  gains go to a wallet that has to do it.
- Emissions. No instruction mints $PIT or pays it out. The program only ever
  burns it.

## Build and test

```
cargo build-sbf
cargo test -p pit-vault --test vault
```

The tests run the compiled program in an in process SVM against a stand in
router that can be told to steal. Each safety check was also removed in turn
to confirm a test fails without it.

## Run on a copy of mainnet

Beyond the tests here, the program has been run against a local validator
holding a copy of the mainnet accounts a real Jupiter swap touches, a buy and
a sell each, on 5 Oct 2026:

| Route | Coin | Transaction | Compute | Call depth |
| --- | --- | --- | --- | --- |
| Whirlpool | WIF, original token program | 1036 bytes | 126K and 118K units | 4 |
| Pump.fun Amm | a graduated pump.fun coin, Token 2022 | 995 and 960 bytes | 201K and 187K units | 4 |

Jupiter builds its instruction for the vault's address without complaint. One
account is exchanged before it is passed through: where Jupiter names the
vault's own wrapped SOL account, the vault program's temporary one goes
instead. The trading key opens the vault's account for the coin beforehand,
since the vault cannot sign a transaction of its own.

## Checking what is deployed

Reading this code only helps if it is the code that is running. Once the
program is on mainnet, a verified build ties the two together: anyone can
rebuild this repository and compare the result with the program at the
address above.

```
solana-verify verify-from-repo --program-id 6PNL53h67LUSn252YL96VY51N6hX5cz1i6Rh3pt9Pmjf <this repository's address>
```

Until that has been done and the result is on record here, treat nothing at
that address as this code.

## Still to prove before mainnet

1. More routes than these, bonding curve coins in particular.
2. An audit by someone who is not the author.


## Using this code

It is published to be read and checked. No licence to reuse it is granted.
