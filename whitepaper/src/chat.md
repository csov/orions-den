# Introduction

It is becoming increasingly common for entities in control of social media to abuse their growing monopoly over information. What we say and listen to is getting gradually more manipulated. It is not unheard of to see people being invisibly silenced and filtered. In some cases even stripped of their job security, because they dared to publicly disagree with what we are supposed to think. Suffice to say, we are slowly getting robbed of our essential rights to speak, and most importantly, to listen freely.

In this paper, we present a decentralized messaging network that gives people option to opt out of the systems in place.

#### Economy

We put emphasis on economical viability. Most popular social media platforms relay on investment incentivized by the amount of users they have. This approach is unsustainable, social platforms are already trying to find ways to actually be profitable, worsening user experience.

We instead create a circular economy where users pay for the usage to the node operators in cryptocurrency. The cryptocurrency needs to have capable enough smart-contracts to implement central fund that periodically finances the operators (treasury). Operators also need to be able to check payment status of users to apply access control. Each feature network offers may use different amount of resources so treasury needs to differentiate and compensate accordingly.

Network needs to protect from misbehaving operators. To become an operator, one needs to stake certain amount to be included in network topology. Operators can vote to slash each other stake based of majority to exclude them from the network. Amount of stake also affects the reward node gets.

Preferably, the prices should be dictated by operators instead of smart-contracts. Topic, time-stamp, and amount should suffice for operators to determine whether the service is financed. This creates competitive market that can keep up with volatile currencies.

#### Privacy and Anonymity

Network offers highest level of privacy:
- the connection to server is authenticated and encrypted with post-quantum algorithms
- user connects to server with their data trough two relays to hide the IP address, payment for relay use and other network features can be form a different wallets
- client, we provide, uses post-quantum secure forward secrecy
- ephemeral messages are deleted upon reading

#### Data Sovereignty

Data that is retained permanently is stored in message block-chain. This protects the message history from being altered and makes it easy to save history to cold storage. The ledger is not global to the network, rather, users can create topic specific block-chains that are validated in parallel by random subset of operators.

#### Accessibility

For client to use all the capabilities for the network, only reliable connection is required. This can be provided by operating system and browser alike. The reference client we provide is fully based on browser technologies and can be hosted as a static website.

#### DDOS protection

In some cases, tracking how much resources users consume is too costly or reduces privacy. In these cases network uses Proof of Work to incur costs to malicious users. For example, anyone can send ephemeral messages to anyone, this means that one can also purposefully fill unread message list with junk to prevent others from sending legitimate messages. If servers only accept messages with valid PoW they can make such attacks hard to maintain.

# Technical Description

Following describes explanation and reasoning behind technical decisions.

## Core encryption primitives

We use Kyber and Falcon for post-quantum secure key exchange and signatures. Each lattice based algorithm is complemented with ecliptic counterpart for redundancy. After key exchange, we use AES-GCM to encrypt messages at transit, because it ensures integrity and authenticity of the messages. For generic hashing we chose Blake3.

```mermaid
flowchart TB
    subgraph key_encapsulation
        EKeypair
        EPublicKey
        Ciphertext
    end
    kyber-->key_encapsulation
    x25519-->key_encapsulation

    subgraph signature_verification
        SKeypair
        SPublicKey
        Signature
    end
    dilitium-->signature_verification
    ed25519-->signature_verification

    subgraph hashing
        Hash
    end
    blake3-->hashing

    subgraph symetric_encryption
        encrypt
        decrypt
    end
    aesGcm-->symetric_encryption

    Nonce-->Proof
    SPublicKey-->Proof
    Signature-->Proof
```

## Post-Quantum Transport Layer Security (PQTLS)

Network uses custom post-quantum handshake based of Falkon for identity verification and Kyber for key exchange. We decided to make our own since we only communicate within the network and standards like **Noise** or **TLS** either aren't post-quantum secure or don't work in browsers (yet). Besides, its trivial to implement.

```mermaid
sequenceDiagram
    participant C as Initiator
    participant S as Responder

    C ->>+ S: Falcon Public Key + Kyber Public Key + Sign Challenge
    S ->>+ C: Falcon Public Key + Kyber Public Key + Proof + Sign Challenge + Kyber Ciphetext
    C ->>+ S: Proof + Kyber Ciphertext
    Note over C,S: Both sides have shared secret and verified each other's identity
```

## Onion routing

To access the network, users need to connect trough decentralized VPN implemented trough onion routing. Since we only need to relay requests to servers within the network, we use custom protocol optimized for this. Key differences form TOR (as an example):

- we always use 2 relays
- we use our own encryption scheme based of Kyber
- the protocol is browser agnostic (we only require reliable connection)
- clients can choose the route, which means they can create routes trough nearby relays (within same continent) to speed up the connection

### Setup

Before sending any messages, **sender** and **recipient** needs to establish a **shared secret** without recipient or relays knowing where client is. This is done with initialization **packet** that has multiple layers of encryption to hide full path from relays and server.

```mermaid
flowchart TB
    subgraph browser
        Client(((client)))
    end

    subgraph "orion network"
        Node1
        Node2
        Recipient(((Recipient)))
    end

    subgraph pfn1["packet encrypted for Node1"]
        na2["Node2 Address"]
        subgraph pfn2["packet encrypted for Node2"]
            na3["Recipient Adress"]
            subgraph pfr["packet encrypted for recipient"]
            end
        end
    end

    Client --> pfn1 --> Node1 --> pfn2 --> Node2 --> pfr --> Recipient
    Client --> Secret
    Recipient --> Secret[[Secret]]
```

## Communication

After the initialization packet gets delivered, both sides have **shared secret** and can simply use AES-GCM to encrypt and decrypt messages on transit. In this case we don't need multiple layers of encryption, since route is established and nodes only forward encrypted stream.

```mermaid
flowchart TB
    Secret[[Secret]]

    subgraph Client
        Message(((Message))) --> aesGcmEncrypt
    end

    subgraph Recipient
        aesGcmDecrypt --> m2(((Message)))
    end

    Secret --> aesGcmDecrypt
    Secret --> aesGcmEncrypt
    aesGcmEncrypt --> Node1 --> Node2 --> aesGcmDecrypt
```

## Distributed Hash Table

Location of any data related to users is stored on the network with deterministic location. Each operator uploads DNS record along their stake so that users can construct full network topology. Full topology allows anyone to compute which nodes store a given **topic**. This is magnitudes more performant the using Dynamic DHT like Kademlia (for our use-case). Nodes can listen for events form the block-chain to always have synchronized topology, while users may cache it and invalidate it anytime they encounter errors related to outdated topology.

## The Message Relay Network

The network provides most flexible, bare minimum to implement highly secure peer to peer, ephemeral messaging. Exposed API makes no assumption about format and encryption to the extent that two clients can not understand each other. This is simpler to implement and allows for more types of clients.

## User Profiles

Main feature of the network is to relay ephemeral messages. Users of the network are identified by their public key that also decides which nodes in the network store their profile information. As long as you know users public key, you know where his profile is, which is necessary to send ephemeral messages.

## Mailbox

Each user profile has associated file (**mailbox**) that stores sequence of **mail**. Mail is simply an arbitrary blob that was sent by another user when profile owner is offline. Nodes cap the mailbox size and sender needs to perform proof of work to send the mail. In other cases, when user is online, the mail is sent directly, without being stored in the mailbox. Owner has permission to read the **mailbox** which clears it. Thus messages sent trough the mail are ephemeral.

```mermaid
sequenceDiagram
    actor S as Sender
    participant SE as Sender Entry
    participant ON as Other Replicators
    participant RS as Recipient Subscription
    actor R as Recipient

    Note over SE,RS: Direct Connections
    RS ->+ R: Offline
    SE -> S: Connect Onion Route

    S ->>+ SE: Mail(PoW, Recipient, Blob)
    SE ->>+ RS: Direct(PoW, Recipient, Blob)
    RS ->>- SE: Error(RecipientOffline)
    RS ->> RS: Store(Blob)
    SE ->> ON: Save(PoW, Recipient, Blob)
    ON ->> ON: Store(Blob)
    SE ->> SE: Store(Blob)
    SE ->>- S: Success(MailSent)

    R ->- RS: Connect Onion Route
    R ->>+ RS: ReadMail(Proof)
    RS ->>+ ON: Replicate(ReadMail(Proof))
    RS ->>+ SE: Replicate(ReadMail(Proof))
    ON ->>- RS: MailList(Blob)
    SE ->>- RS: MailList(Blob)
    RS ->>- R: MailList(Blob)
    
    S ->>+ SE: Mail(PoW, Recipient, Blob)
    SE ->>+ RS: Direct(PoW, Recipient, Blob)
    RS ->> R: Mail(Blob)
    RS ->>- SE: Success(MailSentDirectly)
    SE ->>- S: Success(MailSent)
```

## Profile Vault

User need to be able to store arbitrary data permanently to ensure they don't loose things like friend list and associated secrets (Double-Ratchet). Users might also want to pin arbitrary data to their profile for others to view. For this reason, every profile has associated key value store with limited capacity. Changes to the store are authenticated by signature of Merkle-Tree root that nodes in replication group and users can verify. Values in the vault are opaque to the nodes and clients specify how and what they store there.

# Message Block Chain

Another feature network offers is message block-chain with access control. This form of messaging allows for scalable and archivable community-like chats. This is achieved with distributed random function consensus.

## DRF Consensus

Nodes need to synchronize the order of message history so it has consistent hash, for this one of the nodes is chosen with pseudorandom function seeded by previous block hash. Each node validates the block and send his vote to others. When node accumulate either no or yes majority they will accept or ignore the block. The blocks have variable size to account for failed proposals, after each failure different node is chosen to validate and they wrap around. It is possible that nodes will disagree indivisually, in that case, they start deleting oldest messges as the buffer crosses a trashold to eventually agree on a block. We prefer loosing messages over blocking consenzus forever.

```mermaid
flowchart
    classDef prop stroke:#0a418a,stroke-width:3px
    classDef wait stroke:#8a740a,stroke-width:3px
    classDef voteY stroke:#0a7d06,stroke-width:3px
    classDef voteN stroke:#7d0606,stroke-width:3px

    subgraph Proposal
        direction TB

        CA((A Proposing)):::prop
        CB((B Waiting)):::wait
        CC((C Waiting)):::wait
        CD((D Waiting)):::wait
        CE((E Waiting)):::wait

        CA -->|block| CB
        CA -->|block| CC
        CA -->|block| CD
        CA -->|block| CE

        linkStyle 0,1,2,3 stroke:#0a418a,stroke-width:3px
    end

    Proposal --> Voting

    subgraph Voting
        VA((A Waiting)):::wait
        VB((B Voting)):::voteY
        VC((C Voting)):::voteN
        VD((D Voting)):::voteY
        VE((E Voting)):::voteY

        VB <-->|no| VC
        VC <-->|no| VA
        VC <-->|no| VD
        VC <-->|no| VE

        linkStyle 5,6,7,8 stroke:#7d0606,stroke-width:3px

        VB <-->|yes| VA
        VB <-->|yes| VD
        VB <-->|yes| VE
        VD <-->|yes| VA
        VD <-->|yes| VE
        VE <-->|yes| VA

        linkStyle 9,10,11,12,13,14 stroke:#0a7d06,stroke-width:3px
    end

    subgraph Finalization
        direction TB
        FA((A Finalized)):::prop
        FB((B Finalized)):::prop
        FC(("C Finalized\n(overwritten)")):::wait
        FD((D Finalized)):::prop
        FE((E Finalized)):::prop
    end

    Voting --> Finalization

    subgraph Proposal2
        direction TB

        C2A((A Waiting)):::wait
        C2B((B Waiting)):::wait
        C2C((C Waiting)):::wait
        C2D((D Propoing)):::prop
        C2E((E Waiting)):::wait

        C2D -->|block| C2B
        C2D -->|block| C2C
        C2D -->|block| C2A
        C2D -->|block| C2E

        linkStyle 16,17,18,19 stroke:#0a418a,stroke-width:3px
    end

    Finalization --> Proposal2
```

## Access Control

Other part of the message block-chain is access control. Healthy communities need a way to decide who can send messages, invite users, .etc. Servers therefore maintain member list of a given chat that contains hierarchy of roles. Each member has `rank`, `permissions`, and `ratelimit` that define who's profile they can edit, what actions they can perform, and how often they can perform them. Thus far permissions can limit sending messages, inviting users, and removing members. Nodes need to know about this to properly enforce the rules.

# Network Integrity

We need to account for node churn. For this, both profile and message block-chain includes recovery mechanisms. Whenever node leaves or enters, the topology slightly changes and some node's assigned keys change as well. Recovery of data is initiated when operation on it disagrees with majority. This means nodes are lazy so unused data can naturally disappear over time

# Reference Client

As a way of documenting the network API, we provide reference web client implementation that provides group chats and ephemeral messages. The client is self is split into platform independent library and web interface for the library. Notable part of the client is Double-Ratchet with periodic Kyber key rotation and ephemeral chat ids for optimal session lookup. The ephemeral messaging provided here provides highest level of privacy known to us.

## Post-quantum Double-Ratchet

Double-Ratchet (by Signal) provides forward secrecy for peer-to-peer communication. We use slightly modified version that interleaves the Diffie-Hellman exchanges with periodic Kyber exchange. Both peers take turns thanks to period being odd. We don't simply replace DH with Kyber because each such exchange inflates the message size which "reduces" mailbox capacity. Additionally, DR sessions also maintains rotating asymmetric session id.
